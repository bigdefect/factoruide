-- Profiles shared with the party, for group guides: `/fg share` sends the character's profile
-- (what the app plans a guide from) to the other players of the party; their addon keeps it
-- (`FactoruideDB.shared`) for their app, which can then make a guide for the whole group.
-- Sent only on the player's command; what comes in is checked, never run.

local FG = Factoruide
local T = FG.T

local PREFIX = "FGPROF1"
-- Message sizes are 255 bytes at most: "<id>:<i>:<n>:" then the data.
local CHUNK = 220
-- A profile is this many messages at most (some 40 KB), received within this time (seconds).
local MAX_CHUNKS = 200
local RECEIVE_TIME = 300
-- Nested tables this deep at most.
local MAX_DEPTH = 6
-- Results of C_ChatInfo.SendAddonMessage worth trying again (Enum.SendAddonMessageResult):
-- AddonMessageThrottle, ChannelThrottle, AddOnMessageLockdown.
local RETRY = { [3] = true, [8] = true, [11] = true }

local L = {
    sending = T("profile sent to the party (%d messages, about %d s)"),
    received = T(
        "profile of %s received: reload the interface (/reload) for the Factoruide app to make a group guide with it"
    ),
    alone = T("not in a party: nobody to send the profile to"),
}

local Share = {}
FG.Share = Share

if C_ChatInfo and C_ChatInfo.RegisterAddonMessagePrefix then C_ChatInfo.RegisterAddonMessagePrefix(PREFIX) end

-- Encoding ------------------------------------------------------------------------------------
-- n<number>;  s<length>:<bytes>  t  f  {<key><value>...}

local function Encode(v, out)
    local kind = type(v)
    if kind == "number" then
        table.insert(out, "n" .. tostring(v) .. ";")
    elseif kind == "string" then
        table.insert(out, "s" .. #v .. ":" .. v)
    elseif kind == "boolean" then
        table.insert(out, v and "t" or "f")
    elseif kind == "table" then
        table.insert(out, "{")
        for k, x in pairs(v) do
            local tk, tx = type(k), type(x)
            if (tk == "string" or tk == "number") and tx ~= "function" and tx ~= "userdata" then
                Encode(k, out)
                Encode(x, out)
            end
        end
        table.insert(out, "}")
    end
end

--- The value encoded in `s` from `at`, and where it ends; nil when malformed.
local function Decode(s, at, depth)
    local c = s:sub(at, at)
    if c == "n" then
        local stop = s:find(";", at, true)
        local n = stop and tonumber(s:sub(at + 1, stop - 1))
        if not n then return nil end
        return n, stop + 1
    elseif c == "s" then
        local colon = s:find(":", at, true)
        local len = colon and tonumber(s:sub(at + 1, colon - 1))
        if not len or colon + len > #s then return nil end
        return s:sub(colon + 1, colon + len), colon + len + 1
    elseif c == "t" then
        return true, at + 1
    elseif c == "f" then
        return false, at + 1
    elseif c == "{" then
        if depth >= MAX_DEPTH then return nil end
        local t = {}
        at = at + 1
        while s:sub(at, at) ~= "}" do
            if at > #s then return nil end
            local k, v
            k, at = Decode(s, at, depth + 1)
            if k == nil or type(k) == "boolean" then return nil end
            v, at = Decode(s, at, depth + 1)
            if v == nil then return nil end
            t[k] = v
        end
        return t, at + 1
    end
    return nil
end

function Share.Encode(v)
    local out = {}
    Encode(v, out)
    return table.concat(out)
end

--- The value encoded in `s`, nil when it is not one value exactly.
function Share.Decode(s)
    local ok, v, stop = pcall(Decode, s, 1, 0)
    if ok and v ~= nil and stop == #s + 1 then return v end
    return nil
end

-- Sending -------------------------------------------------------------------------------------

local queue = {}
local ticker

local function Pump()
    while #queue > 0 do
        local result = C_ChatInfo.SendAddonMessage(PREFIX, queue[1], "PARTY")
        if type(result) == "number" and RETRY[result] then return end -- again at the next tick
        table.remove(queue, 1)
        -- One message per tick: under the game's limits.
        if #queue > 0 then return end
    end
    if ticker then
        ticker:Cancel()
        ticker = nil
    end
end

--- `/fg share`: the character's profile, as saved now, to the party.
function Share:Send()
    if not IsInGroup() or IsInRaid() or not C_ChatInfo or not C_ChatInfo.SendAddonMessage then
        FG:Print(L.alone)
        return
    end
    local key = FG:CharacterKey()
    if FG.Profile then FG.Profile:Snapshot() end
    local profile = FactoruideDB.profiles and FactoruideDB.profiles[key]
    if not profile then return end
    local copy = {}
    for k, v in pairs(profile) do
        if k ~= "errors" then copy[k] = v end
    end
    local data = Share.Encode(copy)
    local n = math.ceil(#data / CHUNK)
    local id = tostring(math.random(1000, 9999))
    for i = 1, n do
        table.insert(queue, string.format("%s:%d:%d:%s", id, i, n, data:sub((i - 1) * CHUNK + 1, i * CHUNK)))
    end
    FG:Print(L.sending, n, math.ceil(n * 0.5))
    if not ticker and C_Timer and C_Timer.NewTicker then ticker = C_Timer.NewTicker(0.5, Pump) end
    Pump()
end

-- Receiving -----------------------------------------------------------------------------------

local incoming = {} -- sender -> { id, n, parts, got, since }

--- A profile as the app reads it: the fields it plans from, of the right types.
local function Valid(p)
    return type(p) == "table"
        and type(p.level) == "number"
        and type(p.race) == "number"
        and type(p.class) == "string"
        and type(p.faction) == "string"
        and (p.completed == nil or type(p.completed) == "table")
end

local function Receive(message, sender)
    local key = sender:find("-", 1, true) and sender or (sender .. "-" .. GetNormalizedRealmName())
    if key == FG:CharacterKey() then return end
    local id, i, n, data = message:match("^(%d+):(%d+):(%d+):(.*)$")
    i, n = tonumber(i), tonumber(n)
    if not id or not i or not n or n < 1 or n > MAX_CHUNKS or i < 1 or i > n then return end
    local now = GetTime()
    local p = incoming[key]
    if not p or p.id ~= id or p.n ~= n or now - p.since > RECEIVE_TIME then
        p = { id = id, n = n, parts = {}, got = 0, since = now }
        incoming[key] = p
    end
    if not p.parts[i] then
        p.parts[i] = data
        p.got = p.got + 1
    end
    if p.got < p.n then return end
    incoming[key] = nil
    local profile = Share.Decode(table.concat(p.parts))
    if not Valid(profile) then return end
    -- Its name is who sent it.
    profile.name = key
    profile.received = time()
    FactoruideDB.shared = FactoruideDB.shared or {}
    FactoruideDB.shared[key] = profile
    FG:Print(L.received, (key:gsub("%-.*", "")))
end

FG:On("CHAT_MSG_ADDON", function(prefix, message, _, sender)
    if prefix == PREFIX then Receive(message, sender) end
end)
