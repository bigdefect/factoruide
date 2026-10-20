-- XP compared with the guide: on the game's XP bar, a mark where the guide expects the character
-- to be at the step shown and a band up to where it expects it once the current tasks are done,
-- the gap and the band's XP in the bar's tooltip. The guide draws the same in its own bar
-- (`XpMarker:Shares`).

local FG = Factoruide
local T = FG.T

local XpMarker = {}
FG.XpMarker = XpMarker

local L = {
    ahead = T("Guide: %s xp ahead"),
    behind = T("Guide: %s xp behind"),
    onTime = T("Guide: on schedule"),
    level = T("Guide: level %d planned at this step"),
    span = T("Current tasks: from %s to %s xp"),
}

local mark, markParent, markLine, markEnd, markBand
-- Bars whose OnEnter is hooked.
local hooked = {}

--- Where the guide expects the character before the step shown: its level and XP in it.
function XpMarker:Planned()
    local route = FG.Guide and FG.Guide:Route()
    local state = FG.Guide and FG.Guide:State()
    if not route or not state then return nil end
    local done = route.steps[state.step - 1]
    if done and done.px then return done.lvl, done.px end
    if state.step <= 1 then return route.fromLevel or 1, 0 end
    return nil
end

--- XP `xp` of `level` on the current level's scale (above its size: the next level, below 0: an
--- earlier one), or nil with `level` when it is further away.
local function OnScale(level, xp)
    local here, max = UnitLevel("player"), UnitXPMax("player")
    if level == here then return xp end
    if level == here + 1 then return max + xp end
    if level == here - 1 and xp >= max then return xp - max end
    return nil, level
end

--- The planned XP on the current level's scale (`OnScale`).
function XpMarker:PlannedHere()
    local level, xp = self:Planned()
    if not level then return nil end
    return OnScale(level, xp)
end

--- Where the guide expects the character once the current tasks are done: level and XP in it.
function XpMarker:PlannedAfter()
    local route = FG.Guide and FG.Guide:Route()
    local steps = FG.Guide and FG.Guide:CurrentSteps()
    local last = route and steps and route.steps[steps[#steps]]
    if not last or not last.px then return nil end
    return last.lvl, last.px
end

--- Tooltip line: the XP the current tasks start and end at, on the current level's scale.
function XpMarker:SpanLine()
    local from = self:PlannedHere()
    local level, xp = self:PlannedAfter()
    local to = level and OnScale(level, xp)
    if not from or not to then return nil end
    local n = function(v) return BreakUpLargeNumbers and BreakUpLargeNumbers(v) or tostring(v) end
    return string.format(L.span, n(from), n(to))
end

--- Tooltip line: XP ahead of the guide (positive) or behind it.
function XpMarker:Line()
    local planned, level = self:PlannedHere()
    if not planned then return level and string.format(L.level, level) or nil end
    local gap = UnitXP("player") - planned
    if math.abs(gap) < 1 then return L.onTime end
    local n = BreakUpLargeNumbers and BreakUpLargeNumbers(math.abs(gap)) or tostring(math.abs(gap))
    return string.format(gap > 0 and L.ahead or L.behind, n)
end

local function Usable(f) return type(f) == "table" and f.IsShown and f:IsShown() and (tonumber(f:GetWidth()) or 0) > 50 end

--- The game's own XP bar: on every client now one of the bars of the status bar containers
--- (StatusTrackingBarManager.barContainers, each holding one bar per kind in `bars`, the XP one
--- flagged `isExpBar`). Never the containers themselves: they are not the bar the mouse is on.
local function TrackingXpBar()
    local manager = _G.StatusTrackingBarManager
    local containers = type(manager) == "table" and manager.barContainers
        or { _G.MainStatusTrackingBarContainer, _G.SecondaryStatusTrackingBarContainer }
    local enum = type(StatusTrackingBarInfo) == "table" and StatusTrackingBarInfo.BarsEnum
    local xpIndex = type(enum) == "table" and enum.Experience or nil
    for _, container in pairs(containers) do
        local bars = type(container) == "table" and container.bars
        for index, bar in pairs(type(bars) == "table" and bars or {}) do
            local xp = Usable(bar) and (bar.isExpBar or (xpIndex ~= nil and index == xpIndex))
            -- Shown in a hidden container (another bar in front): not this one.
            if xp and (not bar.IsVisible or bar:IsVisible()) then return bar end
        end
    end
end

--- The XP bar shown: a replacement UI's, the game's, or the older clients' MainMenuExpBar.
local function Bar()
    if Usable(_G.ElvUI_ExperienceBar) then return _G.ElvUI_ExperienceBar end
    local bar = TrackingXpBar()
    if bar then return bar end
    if Usable(_G.MainMenuExpBar) then return _G.MainMenuExpBar end
end

local function Mark(parent)
    if mark and markParent == parent then return mark end
    mark = mark or CreateFrame("Frame", nil, parent)
    mark:SetParent(parent)
    mark:ClearAllPoints()
    mark:SetAllPoints(parent)
    mark:SetFrameLevel((tonumber(parent:GetFrameLevel()) or 1) + 5)
    markParent = parent
    if not markLine then
        markBand = mark:CreateTexture(nil, "ARTWORK")
        markBand:SetColorTexture(1, 0.82, 0, 0.3)
        markLine = mark:CreateTexture(nil, "OVERLAY")
        markLine:SetColorTexture(1, 0.82, 0, 0.95)
        markLine:SetWidth(2)
        markEnd = mark:CreateTexture(nil, "OVERLAY")
        markEnd:SetColorTexture(1, 0.82, 0, 0.6)
        markEnd:SetWidth(2)
    end
    return mark
end

--- The character's XP, where the guide expects it before the step shown and once the current
--- tasks are done, as shares of the current level (0 to 1; the last two nil when unknown).
function XpMarker:Shares()
    local max = UnitXPMax("player")
    if not max or max <= 0 then return 0, nil, nil end
    local function Share(value, level)
        if value then return math.max(0, math.min(1, value / max)) end
        if level then return level > UnitLevel("player") and 1 or 0 end
        return nil
    end
    local from = Share(self:PlannedHere())
    local afterLevel, afterXp = self:PlannedAfter()
    local to = afterLevel and Share(OnScale(afterLevel, afterXp)) or nil
    return math.min(1, UnitXP("player") / max), from, to
end

--- The game's tooltip of the XP bar gets the gap with the guide, added after the bar's own
--- OnEnter filled it. Only a bar that has an OnEnter of its own is hooked: hooking a frame
--- without one gives it one, and the game then has it take the mouse from the frames under it.
local function Hook(bar)
    if hooked[bar] or not bar.HookScript or not bar.GetScript or not bar:GetScript("OnEnter") then return end
    hooked[bar] = true
    bar:HookScript("OnEnter", function()
        local line = FG:Get("uiShown") and FG:Get("xpMarker") and XpMarker:Line()
        if line and GameTooltip:IsShown() then
            GameTooltip:AddLine(line, 1, 0.82, 0)
            local span = XpMarker:SpanLine()
            if span then GameTooltip:AddLine(span, 1, 0.82, 0) end
            GameTooltip:Show()
        end
    end)
end

function XpMarker:Refresh()
    local planned = self:PlannedHere()
    local max = UnitXPMax("player")
    local on = FG:Get("uiShown")
        and FG:Get("xpMarker")
        and FG:Get("guideShown")
        and max
        and max > 0
        and self:Planned() ~= nil
    local bar = on and Bar() or nil
    if mark then mark:SetShown(bar ~= nil) end
    if not bar then return end
    Hook(bar)
    local parent = bar
    local m = Mark(parent)
    local width = tonumber(parent:GetWidth()) or 0
    -- Further than the next or previous level: at the bar's end, the tooltip tells the level.
    local function X(value, level)
        local share = value and value / max or ((level or 0) > UnitLevel("player") and 1 or 0)
        return width * math.max(0, math.min(1, share))
    end
    local from = X(self:PlannedHere())
    local afterLevel, afterXp = self:PlannedAfter()
    local to = afterLevel and X(OnScale(afterLevel, afterXp)) or from
    markLine:ClearAllPoints()
    markLine:SetPoint("TOP", m, "TOPLEFT", from, 2)
    markLine:SetPoint("BOTTOM", m, "BOTTOMLEFT", from, -2)
    -- The current tasks: from the mark to where they end.
    markEnd:ClearAllPoints()
    markEnd:SetPoint("TOP", m, "TOPLEFT", to, 2)
    markEnd:SetPoint("BOTTOM", m, "BOTTOMLEFT", to, -2)
    markEnd:SetShown(to > from + 2)
    markBand:ClearAllPoints()
    markBand:SetPoint("TOPLEFT", m, "TOPLEFT", from, 0)
    markBand:SetPoint("BOTTOMRIGHT", m, "BOTTOMLEFT", math.max(to, from + 1), 0)
    markBand:SetShown(to > from + 1)
    m:Show()
end

for _, event in ipairs({ "PLAYER_XP_UPDATE", "PLAYER_LEVEL_UP", "PLAYER_ENTERING_WORLD" }) do
    FG:On(event, function()
        XpMarker:Refresh()
        if FG.Guide then FG.Guide:Refresh() end
    end)
end
FG:On("PLAYER_LOGIN", function()
    FG:OnSetting("xpMarker", function() XpMarker:Refresh() end)
    FG:OnSetting("uiShown", function() XpMarker:Refresh() end)
end)
