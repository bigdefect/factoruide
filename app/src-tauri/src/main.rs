// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use fg_contrib::contribution::{Contribution, SendState};
use fg_route::Route;
use fg_route::export::diff::{self, Change, RouteDiff};
use fg_route::job::{self, EditCatalog, Options, Overrides, PlanOutcome, PlanRequest, RouteStep, StartState};
use fg_route::model::Names;
use fg_route::terrain::{WalkMemory, Walks};
use fg_route::xp::Edition;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, State};

/// Game database, overrides and addon, embedded at build time (see build.rs).
mod game;

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/embedded.rs"));
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Settings {
    wow_dir: PathBuf,
    /// Game version the app works on (forever, classic, tbc).
    #[serde(default)]
    edition: Edition,
    /// App language (also the default guide language); unset: the system language.
    #[serde(default)]
    language: Option<String>,
    /// Contribution server (its base address); unset: the one the app was built with.
    #[serde(default)]
    contribution_url: Option<String>,
    /// Random id of this install, sent with its contributions (tells how many different
    /// players back what they report; nothing else about the player).
    #[serde(default)]
    contributor: Option<String>,
    /// Send what is new to the contribution server on its own, once a day at most (off unless
    /// the player turns it on).
    #[serde(default)]
    auto_contribute: bool,
    /// Addon behaviour, written to the addon's Config.lua on install.
    #[serde(default)]
    addon: serde_json::Map<String, serde_json::Value>,
    /// What shows the guides in game on WoW Forever (see `guide_addon`).
    #[serde(default)]
    guide_addon: GuideAddon,
    /// Language the guides are written in in the addon (game locale); unset: each guide in the
    /// language it was made in.
    #[serde(default)]
    guide_locale: Option<String>,
    /// Offer every guide to every character, not only those of its race and class.
    #[serde(default)]
    all_guides: bool,
    /// What the addon in the game was last installed with (`addon_snapshot`): the settings page
    /// saves at once, the game gets them when asked.
    #[serde(default)]
    addon_installed: Option<String>,
    /// Whether the addon's setup questions were answered (or skipped): asked once, then on demand.
    #[serde(default)]
    addon_setup_done: bool,
    /// Whether the first launch's questions (where the game is) were answered or skipped.
    #[serde(default)]
    welcome_done: bool,
    /// Characters taken out of the Characters page ("<edition>/<Name-Realm>" -> the time of the
    /// profile then): deleted in game, but the game keeps their files and the addon their
    /// profile. One played again (a newer profile) is back.
    #[serde(default)]
    forgotten_characters: BTreeMap<String, i64>,
}

/// The settings the installed addon depends on (its options, the guides' language, what shows
/// the guides).
fn addon_snapshot(settings: &Settings) -> String {
    serde_json::to_string(&(
        &settings.addon,
        &settings.guide_locale,
        settings.guide_addon(settings.edition),
    ))
    .unwrap_or_default()
}

/// What shows the guides in game: the addon's guide part, or Polestar (WoW Forever only) with a
/// guide pack of them; the addon's profile part records in both cases.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum GuideAddon {
    #[default]
    Factoruide,
    Polestar,
}

use game::{GUIDE_ADDON, PROFILE_ADDON, WowProblem, game_root};

impl Default for Settings {
    fn default() -> Self {
        let wow_dir = if cfg!(windows) {
            PathBuf::from(r"C:\Program Files (x86)\World of Warcraft")
        } else if cfg!(target_os = "macos") {
            PathBuf::from("/Applications/World of Warcraft")
        } else {
            // Under WSL, the Windows game; otherwise Battle.net in Lutris' default Wine prefix.
            let wsl = PathBuf::from("/mnt/c/Program Files (x86)/World of Warcraft");
            match std::env::var_os("HOME") {
                Some(home) if !wsl.is_dir() => {
                    PathBuf::from(home).join("Games/battlenet/drive_c/Program Files (x86)/World of Warcraft")
                }
                _ => wsl,
            }
        };
        Self {
            wow_dir,
            edition: Edition::Forever,
            language: None,
            contribution_url: None,
            contributor: None,
            auto_contribute: false,
            addon: serde_json::Map::new(),
            guide_addon: GuideAddon::Factoruide,
            guide_locale: None,
            all_guides: false,
            addon_installed: None,
            addon_setup_done: false,
            welcome_done: false,
            forgotten_characters: BTreeMap::new(),
        }
    }
}

impl Settings {
    /// The addons folder of a game version's client.
    fn addons_dir(&self, edition: Edition) -> PathBuf {
        edition.client_dir(&self.wow_dir).join("Interface/AddOns")
    }
    /// What shows the guides in a game version: Polestar exists on WoW Forever only.
    fn guide_addon(&self, edition: Edition) -> GuideAddon {
        if edition == Edition::Forever {
            self.guide_addon
        } else {
            GuideAddon::Factoruide
        }
    }
}

struct AppState {
    settings: Mutex<Settings>,
    /// The last install added files the game reads only when it starts.
    restart_needed: Mutex<bool>,
    config_file: PathBuf,
    /// Where the embedded databases are unpacked, and those unpacked so far.
    db_dir: PathBuf,
    dbs: Mutex<HashMap<Edition, PathBuf>>,
    /// Names of the game in each language read so far, to write guides in it.
    names: Mutex<HashMap<(Edition, String), Arc<Names>>>,
    /// Saved guides and debug exports.
    data_dir: PathBuf,
    /// Stops or cancels the generation under way (`stop_plan`, `cancel_plan`).
    plan_interrupt: job::Interrupt,
    /// Walking memory of each edition (paths around the terrain already searched), kept between
    /// generations: filled at the first one with the memories saved with the guides.
    walks: Mutex<HashMap<Edition, Arc<Walks>>>,
    /// The guide version open in the editor.
    editor: Mutex<Option<EditSession>>,
    /// The replays asked of the editor: a newer one makes the one under way give up.
    edit_requests: job::Requests,
    /// The last guide version replayed to be watched (`guide_timeline`): opened again at once.
    timeline: Mutex<Option<(TimelineKey, job::Timeline)>>,
    /// Held while a contribution is read, sent and recorded: one send at a time, manual or
    /// automatic.
    send_lock: Mutex<()>,
    /// Wakes the automatic contributions up (turned on in the settings).
    auto_wake: std::sync::mpsc::Sender<()>,
}

type CmdResult<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    format!("{e:#}")
}

/// Embedded overrides of an edition.
fn overrides(edition: Edition) -> CmdResult<Overrides> {
    Overrides::parse_edition(
        |name| {
            let (_, bytes) = embedded::OVERRIDES.iter().find(|(n, _)| *n == name)?;
            Some(String::from_utf8_lossy(bytes).into_owned())
        },
        edition,
    )
    .map_err(err)
}

impl AppState {
    fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }
    fn edition(&self) -> Edition {
        self.settings.lock().unwrap().edition
    }
    /// The database of an edition, unpacked the first time.
    fn db_path(&self, edition: Edition) -> CmdResult<PathBuf> {
        if let Some(p) = self.dbs.lock().unwrap().get(&edition) {
            return Ok(p.clone());
        }
        let path = unpack_database(&self.db_dir, edition).map_err(err)?;
        self.dbs.lock().unwrap().insert(edition, path.clone());
        Ok(path)
    }
    fn open_db(&self) -> CmdResult<Connection> {
        open_db(&self.db_path(self.edition())?)
    }
    /// Names of the current edition in a game language, read once.
    fn names(&self, locale: &str) -> CmdResult<Arc<Names>> {
        let key = (self.edition(), locale.to_owned());
        if let Some(n) = self.names.lock().unwrap().get(&key) {
            return Ok(n.clone());
        }
        let names = Arc::new(Names::load(&self.open_db()?, locale).map_err(err)?);
        self.names.lock().unwrap().insert(key, names.clone());
        Ok(names)
    }
    /// A guide written in `locale` (None: the language it was made in).
    fn render(&self, route: &Route, locale: Option<&str>) -> CmdResult<Route> {
        let locale = locale.or(route.locale.as_deref()).unwrap_or("enUS");
        Ok(fg_route::export::render(route, &*self.names(locale)?))
    }
}

fn open_db(path: &Path) -> CmdResult<Connection> {
    Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Unpack an edition's embedded database into `dir` once per build (the file name carries its
/// hash), removing the ones of previous builds.
fn unpack_database(dir: &Path, edition: Edition) -> anyhow::Result<PathBuf> {
    use std::hash::{Hash, Hasher};
    let Some((_, data)) = embedded::DATABASES.iter().find(|(e, _)| *e == edition.key()) else {
        anyhow::bail!("no data for {} in this build", edition.rules().edition_name);
    };
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut hasher);
    let prefix = format!("{}-", edition.key());
    let name = format!("{prefix}{:016x}.sqlite", hasher.finish());
    let path = dir.join(&name);
    if !path.exists() {
        std::fs::create_dir_all(dir)?;
        for entry in std::fs::read_dir(dir)?.flatten() {
            let n = entry.file_name().to_string_lossy().into_owned();
            if (n.starts_with(&prefix) || n.starts_with("factoruide-")) && n.ends_with(".sqlite") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        let tmp = dir.join(format!("{name}.tmp"));
        std::fs::write(&tmp, zstd::decode_all(*data)?)?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(path)
}

/// An edition the app knows: whether its data is bundled and its game client installed.
#[derive(Serialize)]
struct EditionInfo {
    key: &'static str,
    name: &'static str,
    /// Data bundled in this build.
    available: bool,
    /// Game client folder found in the WoW folder.
    installed: bool,
    max_level: i64,
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn list_editions(state: State<'_, AppState>) -> Vec<EditionInfo> {
    let settings = state.settings();
    Edition::ALL
        .iter()
        .map(|e| EditionInfo {
            key: e.key(),
            name: e.rules().edition_name,
            available: embedded::DATABASES.iter().any(|(k, _)| *k == e.key()) && e.rules().known(),
            installed: e.installed_in(&settings.wow_dir),
            max_level: e.rules().max_level,
        })
        .collect()
}

/// Routes and results used to live in the Factoruide checkout (`workspace` setting): copy them
/// into the app data folder the first time.
fn migrate_workspace(config: &serde_json::Value, data_dir: &Path) {
    let Some(workspace) = config.get("workspace").and_then(|w| w.as_str()).map(PathBuf::from) else {
        return;
    };
    {
        let folder = "results";
        let (from, to) = (workspace.join(folder), data_dir.join(folder));
        if from.is_dir() && !to.exists() {
            let _ = copy_dir(&from, &to);
        }
    }
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            copy_dir(&path, &to.join(entry.file_name()))?;
        } else {
            std::fs::copy(&path, to.join(entry.file_name()))?;
        }
    }
    Ok(())
}

/// Whether this build checks for updates: only the published ones (`FG_RELEASE` set by the
/// release workflow), not development or local builds, whose version is not a release's.
#[tauri::command]
fn updates_enabled() -> bool {
    option_env!("FG_RELEASE").is_some()
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn get_settings(state: State<'_, AppState>) -> Settings {
    state.settings()
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn set_settings(state: State<'_, AppState>, mut settings: Settings) -> CmdResult<()> {
    let before = state.settings();
    // Not edited by the settings page: kept.
    if settings.contributor.is_none() {
        settings.contributor = before.contributor;
    }
    if settings.addon_installed.is_none() {
        settings.addon_installed = before.addon_installed;
    }
    let wake = settings.auto_contribute && !before.auto_contribute;
    write_settings(&state, settings)?;
    if wake {
        let _ = state.auto_wake.send(());
    }
    Ok(())
}

fn write_settings(state: &AppState, mut settings: Settings) -> CmdResult<()> {
    settings.wow_dir = game_root(&settings.wow_dir);
    let mut current = state.settings.lock().unwrap();
    // Created meanwhile by a contribution (see `contributor_id`): never lost to an older copy.
    if settings.contributor.is_none() {
        settings.contributor.clone_from(&current.contributor);
    }
    save_settings_file(&state.config_file, &settings)?;
    *current = settings;
    Ok(())
}

/// Turn the automatic contributions on or off (only that setting: the pages showing it hold
/// copies of the others).
#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn set_auto_contribute(state: State<'_, AppState>, on: bool) -> CmdResult<()> {
    let mut settings = state.settings();
    let wake = on && !settings.auto_contribute;
    settings.auto_contribute = on;
    write_settings(&state, settings)?;
    if wake {
        let _ = state.auto_wake.send(());
    }
    Ok(())
}

fn save_settings_file(file: &Path, settings: &Settings) -> CmdResult<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(err)?;
    }
    std::fs::write(file, serde_json::to_string_pretty(settings).map_err(err)?).map_err(err)
}

/// Off the main thread: the window stays responsive while the database is unpacked and read.
#[tauri::command]
async fn get_options(app: AppHandle) -> CmdResult<Options> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        job::options(&state.open_db()?, &overrides(state.edition())?).map_err(err)
    })
    .await
    .map_err(err)?
}

/// Save a planning request for debugging (replay with `fg route --request <file>`).
#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn export_request(state: State<'_, AppState>, request: PlanRequest) -> CmdResult<String> {
    let dir = state.data_dir.join("debug");
    std::fs::create_dir_all(&dir).map_err(err)?;
    let name = request
        .name
        .clone()
        .unwrap_or_else(|| format!("{}-{}", request.race, request.class));
    let path = dir.join(format!("request-{name}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&request).map_err(err)?).map_err(err)?;
    Ok(path.display().to_string())
}

/// The game's folder (`wow_dir`, else the saved one) and what is wrong with it for the game
/// version the app works on.
#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn wow_status(state: State<'_, AppState>, wow_dir: Option<PathBuf>) -> game::WowStatus {
    let settings = state.settings();
    game::status(wow_dir.as_deref().unwrap_or(&settings.wow_dir), settings.edition)
}

/// A World of Warcraft folder found on this computer and its game versions' folders.
#[derive(Debug, Serialize)]
struct FoundGame {
    root: PathBuf,
    clients: Vec<String>,
}

/// World of Warcraft folders found on this computer (Battle.net's, the usual places).
#[tauri::command(async)]
fn detect_wow() -> Vec<FoundGame> {
    game::detect()
        .into_iter()
        .map(|root| FoundGame {
            clients: game::client_folders(&root),
            root,
        })
        .collect()
}

/// What the game did with the addon of the game version the app works on: installed by which
/// app version, when it last ran, turned off for some characters, not loaded at all...
#[derive(Debug, Serialize)]
struct AddonState {
    #[serde(flatten)]
    health: game::AddonHealth,
    /// The last install added files the game reads only when it starts: a /reload is not enough.
    restart_needed: bool,
}

#[tauri::command(async)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn addon_health(state: State<'_, AppState>) -> AddonState {
    let settings = state.settings();
    AddonState {
        health: game::addon_health(&settings.wow_dir, settings.edition, ADDON_VERSION),
        restart_needed: *state.restart_needed.lock().unwrap(),
    }
}

/// The .toc files of the addons the app installs and the files each loads ("<folder>" -> list).
fn loaded_files(addons: &Path) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    for folder in [
        PROFILE_ADDON,
        GUIDE_ADDON,
        DATA_ADDON,
        fg_route::export::polestar::ADDON,
    ] {
        for entry in std::fs::read_dir(addons.join(folder)).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "toc")
                && let Ok(toc) = std::fs::read_to_string(&path)
            {
                out.insert(
                    format!("{folder}/{}", entry.file_name().to_string_lossy()),
                    game::toc_files(&toc),
                );
            }
        }
    }
    out
}

// Guides ---------------------------------------------------------------------------
//
// A guide is a name and its versions: every computation (first one, or computed again with the
// same name) adds a version. The guides go to the game all together (the data addon) or one by
// one (the text of `/fg import`), each in the version its page says to follow; the player picks
// a character's guide in game.
//
//   guides/<slug>/guide.json          name, version to follow
//   guides/<slug>/v<N>/meta.json      summary of the version
//   guides/<slug>/v<N>/request.json   configuration
//   guides/<slug>/v<N>/outcome.json   computed guide

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GuideInfo {
    name: String,
    /// Version the characters follow (None: the latest).
    #[serde(default)]
    installed: Option<u32>,
}

/// Summary of one version.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VersionMeta {
    /// Guide folder (slug of the name).
    guide: String,
    version: u32,
    name: String,
    /// Unix seconds.
    created: u64,
    race: String,
    class: String,
    #[serde(default)]
    group: Vec<String>,
    from_level: i64,
    to_level: i64,
    total_time: f64,
    total_time_text: String,
    quests: usize,
    /// Game data used (version of the quest data imported).
    data: Option<String>,
    /// Received from someone else (imported from a .fgguide file).
    #[serde(default)]
    imported: bool,
    /// Edited by hand (from the version before it).
    #[serde(default)]
    edited: bool,
    /// The character met in game the guide starts from ("Name-Realm").
    #[serde(default)]
    character: Option<String>,
}

/// A guide with its versions, newest first.
#[derive(Serialize)]
struct GuideSummary {
    id: String,
    name: String,
    installed: Option<u32>,
    versions: Vec<VersionMeta>,
}

#[derive(Serialize, Deserialize)]
struct GuideVersion {
    meta: VersionMeta,
    request: PlanRequest,
    outcome: PlanOutcome,
}

fn now() -> CmdResult<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(err)?
        .as_secs())
}

impl AppState {
    /// Guides of the current edition (`guides/<edition>/`; the app switches edition only
    /// when no generation is running).
    fn guides(&self) -> PathBuf {
        self.data_dir.join("guides").join(self.edition().key())
    }

    fn guide_info(&self, id: &str) -> CmdResult<GuideInfo> {
        let path = self.guides().join(id).join("guide.json");
        serde_json::from_str(&std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?)
            .map_err(err)
    }

    fn write_guide_info(&self, id: &str, info: &GuideInfo) -> CmdResult<()> {
        let dir = self.guides().join(id);
        std::fs::create_dir_all(&dir).map_err(err)?;
        std::fs::write(dir.join("guide.json"), serde_json::to_string_pretty(info).map_err(err)?).map_err(err)
    }

    fn versions(&self, id: &str) -> Vec<VersionMeta> {
        let mut out: Vec<VersionMeta> = std::fs::read_dir(self.guides().join(id))
            .map(|d| {
                d.flatten()
                    .filter(|e| e.file_name().to_string_lossy().starts_with('v'))
                    .filter_map(|e| {
                        serde_json::from_str(&std::fs::read_to_string(e.path().join("meta.json")).ok()?).ok()
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by_key(|v| std::cmp::Reverse(v.version));
        out
    }

    fn version(&self, id: &str, version: u32) -> CmdResult<GuideVersion> {
        let folder = self.guides().join(id).join(format!("v{version}"));
        let read = |f: &str| std::fs::read_to_string(folder.join(f)).map_err(|e| format!("{f}: {e}"));
        Ok(GuideVersion {
            meta: serde_json::from_str(&read("meta.json")?).map_err(err)?,
            request: serde_json::from_str(&read("request.json")?).map_err(err)?,
            outcome: PlanOutcome::from_json(&read("outcome.json")?).map_err(err)?,
        })
    }

    /// The walking memory of an edition: the first time, the memories of all its guides (each
    /// version's outcome holds the paths it searched).
    fn walks(&self, edition: Edition) -> Arc<Walks> {
        /// Only the memory of a saved outcome.
        #[derive(Deserialize)]
        struct Saved {
            #[serde(default)]
            walks: Option<WalkMemory>,
        }
        if let Some(w) = self.walks.lock().unwrap().get(&edition) {
            return Arc::clone(w);
        }
        let walks = Arc::new(Walks::default());
        let dir = self.data_dir.join("guides").join(edition.key());
        for guide in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            for version in std::fs::read_dir(guide.path()).into_iter().flatten().flatten() {
                let memory = std::fs::read_to_string(version.path().join("outcome.json"))
                    .ok()
                    .and_then(|json| serde_json::from_str::<Saved>(&json).ok())
                    .and_then(|saved| saved.walks);
                if let Some(memory) = memory {
                    walks.add(memory);
                }
            }
        }
        Arc::clone(self.walks.lock().unwrap().entry(edition).or_insert(walks))
    }

    /// Add a version to the guide of this name (created when new).
    fn add_version(
        &self,
        request: &PlanRequest,
        outcome: &PlanOutcome,
        data: Option<String>,
        (imported, edited): (bool, bool),
        created: u64,
    ) -> CmdResult<VersionMeta> {
        let route = &outcome.route;
        let id = job::slug(&route.name);
        let info = self.guide_info(&id).unwrap_or(GuideInfo {
            name: route.name.clone(),
            installed: None,
        });
        self.write_guide_info(
            &id,
            &GuideInfo {
                name: route.name.clone(),
                ..info
            },
        )?;
        let version = self.versions(&id).first().map_or(1, |v| v.version + 1);
        let meta = VersionMeta {
            guide: id.clone(),
            version,
            name: route.name.clone(),
            created,
            race: request.race.clone(),
            class: request.class.clone(),
            group: request.group.clone(),
            from_level: route.from_level,
            to_level: route.to_level,
            total_time: route.total_time,
            total_time_text: route.total_time_text.clone(),
            quests: route.quests,
            data,
            imported,
            edited,
            character: request.start.as_ref().map(|s| s.name.clone()),
        };
        let folder = self.guides().join(&id).join(format!("v{version}"));
        std::fs::create_dir_all(&folder).map_err(err)?;
        std::fs::write(
            folder.join("meta.json"),
            serde_json::to_string_pretty(&meta).map_err(err)?,
        )
        .map_err(err)?;
        std::fs::write(
            folder.join("request.json"),
            serde_json::to_string_pretty(request).map_err(err)?,
        )
        .map_err(err)?;
        std::fs::write(
            folder.join("outcome.json"),
            serde_json::to_string(outcome).map_err(err)?,
        )
        .map_err(err)?;
        Ok(meta)
    }

    /// The version of a guide its characters follow: the one chosen, else the latest.
    fn followed_version(&self, id: &str) -> Option<u32> {
        let chosen = self.guide_info(id).ok()?.installed;
        chosen.or_else(|| self.versions(id).first().map(|v| v.version))
    }

    /// Every guide in the version its page says to follow (the latest unless one is chosen),
    /// written in the language it was made in, with when that version was computed and its
    /// number.
    fn followed_guides(&self) -> CmdResult<Vec<(Route, u64, u32)>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(self.guides()).into_iter().flatten().flatten() {
            let id = entry.file_name().to_string_lossy().into_owned();
            let Some(n) = self.followed_version(&id) else { continue };
            let Ok(v) = self.version(&id, n) else { continue };
            out.push((self.render(&v.outcome.route, None)?, v.meta.created, n));
        }
        out.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        Ok(out)
    }
}

/// Results of earlier versions of the app (`results/<id>/`): each becomes a version of the
/// guide of its name, oldest first.
fn migrate_results(state: &AppState) {
    #[derive(Deserialize)]
    struct OldMeta {
        created: u64,
        data: Option<String>,
        #[serde(default)]
        imported: bool,
    }
    let old = state.data_dir.join("results");
    let Ok(dir) = std::fs::read_dir(&old) else { return };
    let mut found: Vec<(OldMeta, PlanRequest, PlanOutcome)> = dir
        .flatten()
        .filter_map(|e| {
            let read = |f: &str| std::fs::read_to_string(e.path().join(f)).ok();
            Some((
                serde_json::from_str(&read("meta.json")?).ok()?,
                serde_json::from_str(&read("request.json")?).ok()?,
                PlanOutcome::from_json(&read("outcome.json")?).ok()?,
            ))
        })
        .collect();
    found.sort_by_key(|(m, _, _)| m.created);
    for (meta, request, outcome) in &found {
        let _ = state.add_version(
            request,
            outcome,
            meta.data.clone(),
            (meta.imported, false),
            meta.created,
        );
    }
    let _ = std::fs::rename(&old, state.data_dir.join("results.old"));
}

/// Plan a guide on a worker thread, streaming progress as `plan-progress` events. The result
/// is saved as a new version of the guide of the request's name. `stop_plan` ends it early with
/// the best guide found so far; `cancel_plan` makes it fail with "cancelled" (nothing saved).
#[tauri::command]
async fn plan_route(app: AppHandle, request: PlanRequest) -> CmdResult<VersionMeta> {
    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app2.state::<AppState>();
        state.plan_interrupt.reset();
        // The edition the configuration was made for (the current one when unset).
        let edition = request.params.as_ref().map_or_else(|| state.edition(), |p| p.edition);
        let conn = open_db(&state.db_path(edition)?)?;
        let overrides = overrides(edition)?;
        let walks = state.walks(edition);
        let progress = |m: &str| {
            let _ = app.emit("plan-progress", m.to_owned());
        };
        let outcome = job::plan_interruptible(
            &conn,
            &overrides,
            &request,
            &progress,
            &state.plan_interrupt,
            Some(&walks),
        )
        .map_err(|e| {
            if e.is::<job::Cancelled>() {
                job::Cancelled.to_string()
            } else {
                err(e)
            }
        })?;
        state.add_version(&request, &outcome, data_version(&conn), (false, false), now()?)
    })
    .await
    .map_err(err)?
}

/// Version of the quest data imported in a database.
fn data_version(conn: &Connection) -> Option<String> {
    conn.query_row("SELECT detail FROM import_run WHERE source = 'questie'", [], |r| {
        r.get::<_, Option<String>>(0)
    })
    .ok()
    .flatten()
}

// Guide editor -----------------------------------------------------------------------
//
// A version opened in the editor is loaded once (character, settings, world); each change of
// its steps is replayed (`edit_simulate`), nothing is written until `edit_save` adds the edited
// guide as a new version.

struct EditSession {
    edition: Edition,
    request: PlanRequest,
    notes: Vec<String>,
    editor: job::Editor,
}

/// A version opened in the editor.
#[derive(Serialize)]
struct EditOpened {
    steps: Vec<RouteStep>,
    /// Steps read back from the guide (computed before the editor): its time may differ a little.
    rebuilt: bool,
    /// The steps replayed.
    view: job::EditView,
    catalog: EditCatalog,
}

/// A guide version replayed (game version, guide, version, when it was computed, language).
type TimelineKey = (Edition, String, u32, u64, String);

/// A version replayed to be watched: when each step starts and ends, the character's moves and
/// what it has after each step (`locale`: language of the names).
#[tauri::command]
async fn guide_timeline(app: AppHandle, guide: String, version: u32, locale: String) -> CmdResult<job::Timeline> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let v = state.version(&guide, version)?;
        let edition = v.request.params.as_ref().map_or_else(|| state.edition(), |p| p.edition);
        let key: TimelineKey = (edition, guide, version, v.meta.created, locale);
        if let Some((k, timeline)) = state.timeline.lock().unwrap().as_ref()
            && *k == key
        {
            return Ok(timeline.clone());
        }
        let conn = open_db(&state.db_path(edition)?)?;
        let walks = state.walks(edition);
        let editor = job::Editor::open(&conn, &overrides(edition)?, &v.request, Some(&walks)).map_err(err)?;
        let timeline = editor.timeline(&v.outcome, &*state.names(&key.4)?);
        *state.timeline.lock().unwrap() = Some((key, timeline.clone()));
        Ok(timeline)
    })
    .await
    .map_err(err)?
}

/// Open a version in the editor (`locale`: language of the lines and names shown).
#[tauri::command]
async fn edit_open(app: AppHandle, guide: String, version: u32, locale: String) -> CmdResult<EditOpened> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let v = state.version(&guide, version)?;
        let edition = v.request.params.as_ref().map_or_else(|| state.edition(), |p| p.edition);
        let conn = open_db(&state.db_path(edition)?)?;
        let walks = state.walks(edition);
        let editor = job::Editor::open(&conn, &overrides(edition)?, &v.request, Some(&walks)).map_err(err)?;
        let names = state.names(&locale)?;
        let (steps, rebuilt) = editor.steps(&v.outcome);
        let request = state.edit_requests.next();
        let opened = EditOpened {
            view: editor.view(&steps, &names, &request).ok_or(SUPERSEDED)?,
            catalog: editor.catalog(&names),
            steps,
            rebuilt,
        };
        *state.editor.lock().unwrap() = Some(EditSession {
            edition,
            request: v.request,
            notes: v.outcome.notes,
            editor,
        });
        Ok(opened)
    })
    .await
    .map_err(err)?
}

/// What a replay of the editor answers when a newer one was asked for meanwhile.
const SUPERSEDED: &str = "superseded";

/// Replay the steps of the guide open in the editor: its report, its lines and their timeline.
/// The replay under way gives up for this one (it then fails with "superseded").
#[tauri::command]
async fn edit_simulate(app: AppHandle, steps: Vec<RouteStep>, locale: String) -> CmdResult<job::EditView> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let request = state.edit_requests.next();
        let names = state.names(&locale)?;
        let session = state.editor.lock().unwrap();
        let session = session.as_ref().ok_or("no guide open in the editor")?;
        session
            .editor
            .view(&steps, &names, &request)
            .ok_or_else(|| SUPERSEDED.to_owned())
    })
    .await
    .map_err(err)?
}

/// What could be added to the steps of the guide open in the editor after its step `after`
/// (none: before the first): quests not taken, objectives and turn-ins the steps forget. Gives
/// up for a newer replay or suggestion (failing with "superseded").
#[tauri::command]
async fn edit_suggest(
    app: AppHandle,
    steps: Vec<RouteStep>,
    after: Option<usize>,
    locale: String,
) -> CmdResult<job::Suggestions> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let request = state.edit_requests.next();
        let names = state.names(&locale)?;
        let session = state.editor.lock().unwrap();
        let session = session.as_ref().ok_or("no guide open in the editor")?;
        (session.editor.suggest(&steps, after, &names, &request)).ok_or_else(|| SUPERSEDED.to_owned())
    })
    .await
    .map_err(err)?
}

/// Save the edited steps as a new version of the guide (refused while a step is not possible)
/// and close the editor.
#[tauri::command]
async fn edit_save(app: AppHandle, steps: Vec<RouteStep>) -> CmdResult<VersionMeta> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let mut session = state.editor.lock().unwrap();
        let open = session.as_ref().ok_or("no guide open in the editor")?;
        let outcome = open.editor.save(&steps, open.notes.clone()).map_err(err)?;
        let data = data_version(&open_db(&state.db_path(open.edition)?)?);
        let meta = state.add_version(&open.request, &outcome, data, (false, true), now()?)?;
        *session = None;
        Ok(meta)
    })
    .await
    .map_err(err)?
}

/// Leave the editor without saving.
#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn edit_close(state: State<'_, AppState>) {
    *state.editor.lock().unwrap() = None;
}

/// End the optimization under way now: `plan_route` saves and returns the best guide so far.
#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn stop_plan(state: State<'_, AppState>) {
    state.plan_interrupt.stop();
}

/// Give the generation under way up: `plan_route` fails with "cancelled" within a moment.
#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn cancel_plan(state: State<'_, AppState>) {
    state.plan_interrupt.cancel();
}

fn summary(state: &AppState, id: &str) -> CmdResult<GuideSummary> {
    let info = state.guide_info(id)?;
    Ok(GuideSummary {
        id: id.to_owned(),
        name: info.name,
        installed: info.installed,
        versions: state.versions(id),
    })
}

/// Every guide, most recently computed first.
#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn list_guides(state: State<'_, AppState>) -> Vec<GuideSummary> {
    let Ok(dir) = std::fs::read_dir(state.guides()) else {
        return Vec::new();
    };
    let mut out: Vec<GuideSummary> = dir
        .flatten()
        .filter_map(|e| summary(&state, &e.file_name().to_string_lossy()).ok())
        .filter(|g| !g.versions.is_empty())
        .collect();
    out.sort_by(|a, b| b.versions[0].created.cmp(&a.versions[0].created));
    out
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn get_guide(state: State<'_, AppState>, id: String) -> CmdResult<GuideSummary> {
    summary(&state, &id)
}

/// Two versions of a guide compared: both, their differences, and the same as text.
#[derive(Serialize)]
struct Comparison {
    a: GuideVersion,
    b: GuideVersion,
    diff: RouteDiff,
    /// Settings that differ (the request, the game data).
    settings: Vec<Change>,
    /// The comparison as text (`fg diff`), to hand to someone looking for what went wrong.
    text: String,
}

/// Versions `a` and `b` of a guide compared, written in `locale` (game locale).
#[tauri::command(async)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn compare_versions(
    state: State<'_, AppState>,
    guide: String,
    a: u32,
    b: u32,
    locale: Option<String>,
) -> CmdResult<Comparison> {
    let load = |n: u32| -> CmdResult<GuideVersion> {
        let mut v = state.version(&guide, n)?;
        v.outcome.walks = None;
        v.outcome.plan.clear();
        Ok(v)
    };
    let (mut a, mut b) = (load(a)?, load(b)?);
    let settings = |v: &GuideVersion| -> CmdResult<serde_json::Value> {
        let mut value = serde_json::to_value(&v.request).map_err(err)?;
        if let serde_json::Value::Object(map) = &mut value {
            map.insert("data".into(), v.meta.data.clone().into());
        }
        Ok(value)
    };
    let settings = diff::value_changes(&settings(&a)?, &settings(&b)?);
    // Compared as saved (zones by their English names), shown in `locale`.
    let d = diff::diff(&a.outcome.route, &b.outcome.route);
    let names = [format!("v{}", a.meta.version), format!("v{}", b.meta.version)];
    let text = diff::to_text(
        &d,
        &a.outcome.route,
        &b.outcome.route,
        [&names[0], &names[1]],
        &settings,
    );
    if let Some(locale) = &locale {
        a.outcome.route = state.render(&a.outcome.route, Some(locale))?;
        b.outcome.route = state.render(&b.outcome.route, Some(locale))?;
    }
    Ok(Comparison {
        a,
        b,
        diff: d,
        settings,
        text,
    })
}

#[tauri::command(async)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn get_version(
    state: State<'_, AppState>,
    guide: String,
    version: u32,
    locale: Option<String>,
) -> CmdResult<GuideVersion> {
    let mut v = state.version(&guide, version)?;
    // The walking memory and the plan stay in the files (the app shows neither).
    v.outcome.walks = None;
    v.outcome.plan.clear();
    if let Some(locale) = locale {
        v.outcome.route = state.render(&v.outcome.route, Some(&locale))?;
    }
    Ok(v)
}

/// The version of a guide its characters follow (`None`: the latest), written to the addon.
#[tauri::command(async)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn set_installed(state: State<'_, AppState>, guide: String, version: Option<u32>) -> CmdResult<()> {
    let info = state.guide_info(&guide)?;
    state.write_guide_info(
        &guide,
        &GuideInfo {
            installed: version,
            ..info
        },
    )
}

#[tauri::command(async)]
fn delete_version(state: State<'_, AppState>, guide: String, version: u32) -> CmdResult<()> {
    std::fs::remove_dir_all(state.guides().join(&guide).join(format!("v{version}"))).map_err(err)?;
    let info = state.guide_info(&guide)?;
    if state.versions(&guide).is_empty() {
        delete_guide(state, guide)
    } else {
        if info.installed == Some(version) {
            state.write_guide_info(
                &guide,
                &GuideInfo {
                    installed: None,
                    ..info
                },
            )?;
        }
        Ok(())
    }
}

#[tauri::command(async)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn delete_guide(state: State<'_, AppState>, guide: String) -> CmdResult<()> {
    std::fs::remove_dir_all(state.guides().join(&guide)).map_err(err)
}

use job::DATA_ADDON;

/// The oldest addon this app works with (its .toc version): older ones are to be updated.
const ADDON_VERSION: &str = embedded::ADDON_VERSION;

/// Write the guides assigned to a character (in the version they follow) and who follows which,
/// the places of the trainers, vendors... and what the contribution server has (the addon drops
/// those recordings) into the data addon; on WoW Forever with Polestar, its guide pack too. The
/// addon itself comes from an addon site and is set up in game. Returns the number of guides.
#[tauri::command(async)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn install_addon(state: State<'_, AppState>) -> CmdResult<usize> {
    write_data(&state)
}

/// What `install_addon` does.
fn write_data(state: &AppState) -> CmdResult<usize> {
    // Off the main thread, two writes could touch the same files at once: one at a time.
    static INSTALLING: Mutex<()> = Mutex::new(());
    let _one = INSTALLING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let settings = state.settings();
    match game::status(&settings.wow_dir, settings.edition).problem {
        None => {}
        Some(WowProblem::NotWritable) => {
            return Err(format!(
                "the app may not write in {}: give your Windows user the right to modify the World of \
                 Warcraft folder (its Properties, Security tab), or start the app once as administrator",
                settings.addons_dir(settings.edition).display()
            ));
        }
        Some(_) => {
            return Err(format!(
                "{} not found in {}: choose the World of Warcraft folder in Settings",
                settings.edition.rules().edition_name,
                settings.wow_dir.display()
            ));
        }
    }
    let addons = settings.addons_dir(settings.edition);
    let before = loaded_files(&addons);
    migrate_legacy_addon(&addons);
    let guides = state.followed_guides()?;
    let count = guides.len();
    let mut lua = job::guides_lua(
        &state.open_db()?,
        &overrides(settings.edition)?,
        settings.edition,
        guides,
    )
    .map_err(err)?;
    lua.push('\n');
    let sent = read_send_record(&send_record_file(&state.data_dir, settings.edition)).unwrap_or_default();
    lua.push_str(&sent_lua(&sent.state));
    let guide_toc = addons.join(GUIDE_ADDON).join(format!("{GUIDE_ADDON}.toc"));
    let interface = std::fs::read_to_string(&guide_toc)
        .ok()
        .and_then(|toc| toc.lines().find(|l| l.starts_with("## Interface:")).map(str::to_owned))
        .unwrap_or_else(|| embedded::ADDON_INTERFACE.to_owned());
    job::write_data_addon(
        &addons.join(DATA_ADDON),
        &lua,
        &interface,
        env!("CARGO_PKG_VERSION"),
        now()?,
    )
    .map_err(err)?;
    // A folder or a file the game does not load yet: it reads the .toc files when it starts.
    let after = loaded_files(&addons);
    let restart = after.iter().any(|(toc, files)| before.get(toc) != Some(files));
    *state.restart_needed.lock().unwrap() = restart;
    write_settings(
        state,
        Settings {
            addon_installed: Some(addon_snapshot(&settings)),
            ..settings
        },
    )?;
    Ok(count)
}

/// WoW Forever: every guide (see `followed_guides`) as a guide pack for Polestar, written into
/// the game (Polestar picks each character's guide by its race, class and level). Returns the
/// number of guides.
#[tauri::command(async)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn install_polestar(state: State<'_, AppState>) -> CmdResult<usize> {
    let settings = state.settings();
    if settings.edition != Edition::Forever {
        return Err("Polestar is for WoW Forever only".into());
    }
    let guides = state
        .followed_guides()?
        .into_iter()
        .map(|(route, _, _)| (job::slug(&route.name), route))
        .collect::<Vec<_>>();
    let pack = settings
        .addons_dir(Edition::Forever)
        .join(fg_route::export::polestar::ADDON);
    let files = fg_route::export::polestar::pack(&guides, env!("CARGO_PKG_VERSION"));
    let _ = std::fs::remove_dir_all(&pack);
    std::fs::create_dir_all(&pack).map_err(err)?;
    for (name, content) in files {
        std::fs::write(pack.join(name), content).map_err(err)?;
    }
    *state.restart_needed.lock().unwrap() = true;
    Ok(guides.len())
}

/// What the player pastes in game with `/fg import`: one guide version (the addon adds it to
/// the guides it has, or replaces the one of its name).
#[tauri::command(async)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn guide_text(state: State<'_, AppState>, guide: String, version: u32) -> CmdResult<String> {
    let v = state.version(&guide, version)?;
    let edition = v.request.params.as_ref().map_or_else(|| state.edition(), |p| p.edition);
    let route = state.render(&v.outcome.route, None)?;
    let lua = job::guides_lua(
        &state.open_db()?,
        &overrides(edition)?,
        edition,
        vec![(route, v.meta.created, version)],
    )
    .map_err(err)?;
    Ok(fg_route::export::fgstring::encode(
        fg_route::export::fgstring::GUIDE_PREFIX,
        lua.as_bytes(),
    ))
}

/// Whether Polestar is installed in WoW Forever (it shows guides there only).
#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn polestar_installed(state: State<'_, AppState>) -> bool {
    polestar_installed_in(&state.settings())
}

fn polestar_installed_in(settings: &Settings) -> bool {
    settings
        .addons_dir(Edition::Forever)
        .join("Polestar/Polestar.toc")
        .exists()
}

/// Export a guide version for Polestar's own tools (route planner, playground): its `.guide`
/// file (in English) added to the pack folder in `dir` (the folder chosen, or a pack folder made
/// in it), the pack's other guides kept. Returns the pack's folder. Polestar exists on WoW
/// Forever only.
#[tauri::command(async)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn export_polestar(state: State<'_, AppState>, guide: String, version: u32, dir: String) -> CmdResult<String> {
    if state.edition() != Edition::Forever {
        return Err("Polestar is for WoW Forever only".into());
    }
    let route = state.render(&state.version(&guide, version)?.outcome.route, Some("enUS"))?;
    let pack = job::polestar_source_dir(Path::new(&dir));
    job::add_polestar_source(&pack, &guide, &route, env!("CARGO_PKG_VERSION")).map_err(err)?;
    Ok(pack.display().to_string())
}

/// Whether the data addon in the game has the current guides (else it is to be written again).
#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn addon_synced(state: State<'_, AppState>) -> bool {
    let settings = state.settings();
    let addons = settings.addons_dir(settings.edition);
    addons.join(DATA_ADDON).join("Data.lua").exists()
        && settings.addon_installed.as_deref() == Some(addon_snapshot(&settings).as_str())
}

/// Pages the app opens in the browser: the project's (repository, license, notice) and the
/// community tools the contribution page points to; nothing else.
const LINKS: &[&str] = &[
    "https://github.com/brouznouf/factoruide/",
    "https://www.curseforge.com/wow/",
    "https://github.com/Questie/QuestieTrace",
    "https://questie.dev/trace",
    "https://www.wowhead.com/",
];

/// Open one of the `LINKS` pages in the browser.
#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn open_link(url: String) -> CmdResult<()> {
    if !LINKS.iter().any(|l| url.starts_with(l)) {
        return Err(format!("not a link the app opens: {url}"));
    }
    let program = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(program).arg(&url).spawn().map_err(err)?;
    Ok(())
}

/// A Lua string literal.
fn lua_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            // Other control characters as decimal escapes, always 3 digits (a digit may follow).
            c if c.is_ascii_control() => {
                let _ = write!(out, "\\{:03}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The addon was called BrouzQuest, then was a single addon, Factoruide, before its profile and
/// guide parts: drop their folders (the game would load them too) and carry their SavedVariables
/// over to the profile part. `addons`: the client's Interface/AddOns folder.
fn migrate_legacy_addon(addons: &Path) {
    let _ = std::fs::remove_dir_all(addons.join("BrouzQuest"));
    let _ = std::fs::remove_dir_all(addons.join("Factoruide"));
    if let Some(client) = addons.parent().and_then(Path::parent) {
        migrate_saved_variables(client);
    }
}

/// The SavedVariables of the former addons, carried over to the profile part, unless that one
/// has newer ones (a game still running the old addon writes its file until it restarts: carried
/// over again then). The old files are kept as `.migrated`.
fn migrate_saved_variables(client: &Path) {
    let Ok(accounts) = std::fs::read_dir(client.join("WTF/Account")) else {
        return;
    };
    let modified = |f: &Path| std::fs::metadata(f).and_then(|m| m.modified()).ok();
    for account in accounts.flatten() {
        let dir = account.path().join("SavedVariables");
        let (brouzquest, single, profile) = (
            dir.join("BrouzQuest.lua"),
            dir.join("Factoruide.lua"),
            dir.join(format!("{PROFILE_ADDON}.lua")),
        );
        if !single.exists()
            && !profile.exists()
            && let Ok(text) = std::fs::read_to_string(&brouzquest)
            && std::fs::write(&single, text.replace("BrouzQuestDB", "FactoruideDB")).is_ok()
        {
            let _ = std::fs::rename(&brouzquest, dir.join("BrouzQuest.lua.migrated"));
        }
        if single.exists()
            && (!profile.exists() || modified(&single) > modified(&profile))
            && std::fs::copy(&single, &profile).is_ok()
        {
            let _ = std::fs::rename(&single, dir.join("Factoruide.lua.migrated"));
        }
    }
}

/// What a .fgguide (formerly .bqroute) file holds: a guide version (configuration and computed
/// guide) to share.
#[derive(Serialize, Deserialize)]
struct SharedGuide {
    format: String,
    version: u32,
    meta: serde_json::Value,
    request: PlanRequest,
    outcome: PlanOutcome,
}

const SHARE_FORMAT: &str = "factoruide-guide";
/// Format of the files shared before the rename (BrouzQuest), still accepted.
const LEGACY_SHARE_FORMAT: &str = "brouzquest-result";

/// Write a version as a .fgguide file in the Downloads folder; returns its path.
#[tauri::command(async)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn export_version(app: AppHandle, state: State<'_, AppState>, guide: String, version: u32) -> CmdResult<String> {
    let v = state.version(&guide, version)?;
    let dir = app.path().download_dir().map_err(err)?;
    let path = dir.join(format!("{guide}-v{version}.fgguide"));
    let shared = SharedGuide {
        format: SHARE_FORMAT.into(),
        version: 2,
        meta: serde_json::to_value(&v.meta).map_err(err)?,
        request: v.request,
        outcome: v.outcome,
    };
    std::fs::write(&path, serde_json::to_string(&shared).map_err(err)?).map_err(err)?;
    Ok(path.display().to_string())
}

/// Add a guide received as a .fgguide file (its content): a new version of the guide of its name.
#[tauri::command(async)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn import_guide(state: State<'_, AppState>, content: String) -> CmdResult<VersionMeta> {
    let mut shared: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| format!("not a Factoruide guide file: {e}"))?;
    // A guide made by another version of the app, in the format of this one.
    if let Some(outcome) = shared.get_mut("outcome") {
        let o = PlanOutcome::from_value(outcome.take()).map_err(err)?;
        *outcome = serde_json::to_value(o).map_err(err)?;
    }
    let shared: SharedGuide =
        serde_json::from_value(shared).map_err(|e| format!("not a Factoruide guide file: {e}"))?;
    if shared.format != SHARE_FORMAT && shared.format != LEGACY_SHARE_FORMAT {
        return Err("not a Factoruide guide file".into());
    }
    let data = shared.meta.get("data").and_then(|d| d.as_str()).map(str::to_owned);
    // Its walking memory joins the app's (when made on the same terrain data).
    if let (Some(memory), Some(walks)) = (&shared.outcome.walks, state.walks.lock().unwrap().get(&state.edition())) {
        walks.add(memory.clone());
    }
    state.add_version(&shared.request, &shared.outcome, data, (true, false), now()?)
}

// Characters -------------------------------------------------------------------------

/// A character the addon recorded (`/fg profile`, or at each logout), to start a guide from.
#[derive(Serialize)]
struct CharacterInfo {
    /// Full name when the game gives characters one (WoW Forever: first and last name), from the
    /// client's per-character folders (`WTF/Account/<account>/<realm>/<First>-<Last>/`).
    full_name: Option<String>,
    /// Race key of `races.toml` (None: a race this game version does not know).
    race: Option<String>,
    class: String,
    /// Professions it has, by key, with their skill.
    professions: Vec<(String, i64)>,
    profile: StartState,
}

/// Rested XP gained while logged out since the profile was saved: 5% of a level every 8 hours
/// in an inn or a city, a quarter of it elsewhere, up to a level and a half.
fn rested_now(p: &StartState, now: i64) -> i64 {
    if p.xp_max <= 0 || p.time <= 0 {
        return p.rested;
    }
    let hours = (now - p.time).max(0) as f64 / 3600.0;
    let per_hour = 0.05 / 8.0 * p.xp_max as f64 * if p.resting { 1.0 } else { 0.25 };
    let cap = p.xp_max * 3 / 2;
    (p.rested + (hours * per_hour) as i64).min(cap.max(p.rested))
}

/// The characters of the game client of the current edition, most recently played first.
// Off the main thread: reading the SavedVariables can take a while.
#[tauri::command]
async fn list_characters(app: AppHandle) -> CmdResult<Vec<CharacterInfo>> {
    tauri::async_runtime::spawn_blocking(move || characters(&app.state::<AppState>()))
        .await
        .map_err(err)?
}

/// Full names of the characters by first name, from the per-character folders of the accounts
/// of these SavedVariables files (`<account>/<realm>/<First>-<Last>/`).
fn full_names(files: &[PathBuf]) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for account in files.iter().filter_map(|f| f.parent()?.parent()) {
        for realm in std::fs::read_dir(account).into_iter().flatten().flatten() {
            for character in std::fs::read_dir(realm.path()).into_iter().flatten().flatten() {
                let folder = character.file_name().to_string_lossy().into_owned();
                if let Some((first, last)) = folder.split_once('-')
                    && character.path().is_dir()
                {
                    let full = format!("{first} {last}");
                    let names = out.entry(first.to_owned()).or_default();
                    if !names.contains(&full) {
                        names.push(full);
                    }
                }
            }
        }
    }
    out
}

/// Key of a character in `Settings::forgotten_characters`.
fn forgotten_key(edition: Edition, character: &str) -> String {
    format!("{}/{character}", edition.key())
}

/// Whether a character's profile was taken out of the list (and not played since).
fn is_forgotten(forgotten: &BTreeMap<String, i64>, edition: Edition, profile: &StartState) -> bool {
    forgotten
        .get(&forgotten_key(edition, &profile.name))
        .is_some_and(|&time| profile.time <= time)
}

/// Take a character out of the Characters page (deleted in game): it no longer follows a guide.
/// Played again, it is back.
#[tauri::command(async)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn forget_character(state: State<'_, AppState>, character: String) -> CmdResult<()> {
    let time = characters(&state)?
        .into_iter()
        .find(|c| c.profile.name == character)
        .map_or(i64::MAX, |c| c.profile.time);
    let mut settings = state.settings();
    settings
        .forgotten_characters
        .insert(forgotten_key(settings.edition, &character), time);
    write_settings(&state, settings)
}

fn characters(state: &AppState) -> CmdResult<Vec<CharacterInfo>> {
    let settings = state.settings();
    let client_dir = settings.edition.client_dir(&settings.wow_dir);
    let files = fg_contrib::collector::find_saved_variables(&client_dir);
    let full_names = full_names(&files);
    let races = overrides(settings.edition)?.races;
    let now = i64::try_from(now()?).map_err(err)?;
    Ok(fg_contrib::profile::read(&files)
        .map_err(err)?
        .into_iter()
        .filter_map(|json| serde_json::from_value::<StartState>(json).ok())
        .filter(|profile| !is_forgotten(&settings.forgotten_characters, settings.edition, profile))
        .map(|mut profile| {
            profile.rested = rested_now(&profile, now);
            // Only when the first name is not shared (two characters with the same one).
            let first = profile.name.split('-').next().unwrap_or_default();
            let full_name = match full_names.get(first).map(Vec::as_slice) {
                Some([one]) => Some(one.clone()),
                _ => None,
            };
            CharacterInfo {
                full_name,
                race: races
                    .iter()
                    .find(|(_, r)| r.id == profile.race)
                    .map(|(key, _)| key.clone()),
                class: profile.class.to_lowercase(),
                professions: profile
                    .professions
                    .iter()
                    .filter_map(|s| {
                        let def = fg_route::profession::PROFESSIONS
                            .iter()
                            .find(|d| d.skill_id == s.line)?;
                        Some((def.key.to_owned(), s.rank))
                    })
                    .collect(),
                profile,
            }
        })
        .collect())
}

// Maps -----------------------------------------------------------------------------

/// Map index written by `fg maps` (ids, names, sizes, world bounds).
#[tauri::command(async)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn map_index(state: State<'_, AppState>) -> CmdResult<serde_json::Value> {
    let db = state.open_db()?;
    let text: String = db
        .query_row("SELECT value FROM app_meta WHERE key = 'map_index'", [], |r| r.get(0))
        .map_err(err)?;
    let mut index: serde_json::Value = serde_json::from_str(&text).map_err(err)?;
    // Zone names in the game's languages (`names`: locale -> name), when the data has them.
    let mut names: HashMap<i64, serde_json::Map<String, serde_json::Value>> = HashMap::new();
    if let Ok(mut stmt) =
        db.prepare("SELECT id, locale, name FROM l10n WHERE entity_type = 'zone' AND name IS NOT NULL")
    {
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            })
            .map_err(err)?;
        for (id, locale, name) in rows.flatten() {
            names.entry(id).or_default().insert(locale, name.into());
        }
    }
    for map in index.as_array_mut().into_iter().flatten() {
        if let Some(n) = map["area"].as_i64().and_then(|a| names.get(&a)) {
            map["names"] = serde_json::Value::Object(n.clone());
        }
    }
    Ok(index)
}

/// A map image as a data URL.
#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn map_image(state: State<'_, AppState>, id: i64) -> CmdResult<tauri::ipc::Response> {
    let name = format!("{}/{id}.webp", state.edition().key());
    let (_, bytes) = embedded::MAPS
        .iter()
        .find(|(n, _)| *n == name)
        .ok_or_else(|| format!("no map image {name}"))?;
    Ok(tauri::ipc::Response::new(bytes.to_vec()))
}

/// An edition's minimap pack (`fg minimap`): its index, `(map, level, x, y)` -> (offset, length)
/// in the images after it.
struct Minimap {
    tiles: HashMap<(i64, u32, u32, u32), (usize, usize)>,
    images: &'static [u8],
}

fn minimap(edition: Edition) -> Option<&'static Minimap> {
    static PACKS: std::sync::OnceLock<HashMap<&'static str, Minimap>> = std::sync::OnceLock::new();
    let packs = PACKS.get_or_init(|| {
        embedded::MINIMAPS
            .iter()
            .filter_map(|&(edition, pack)| {
                let len = u32::from_le_bytes(pack.get(4..8)?.try_into().ok()?) as usize;
                if pack.get(..4)? != b"FGMM" {
                    return None;
                }
                let index: Vec<(i64, u32, u32, u32, usize, usize)> =
                    serde_json::from_slice(pack.get(8..8 + len)?).ok()?;
                let tiles = index
                    .into_iter()
                    .map(|(m, l, x, y, o, n)| ((m, l, x, y), (o, n)))
                    .collect();
                Some((
                    edition,
                    Minimap {
                        tiles,
                        images: &pack[8 + len..],
                    },
                ))
            })
            .collect()
    });
    packs.get(edition.key())
}

/// The minimap's images of the current game version: `[map, level, x, y]` of each (none
/// without a minimap).
#[tauri::command(async)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn minimap_index(state: State<'_, AppState>) -> Vec<(i64, u32, u32, u32)> {
    let mut tiles: Vec<_> = minimap(state.edition())
        .map(|m| m.tiles.keys().copied().collect())
        .unwrap_or_default();
    tiles.sort_unstable();
    tiles
}

/// A minimap image (WebP): of game map `map`, at `level` (2^level ADT tiles a side), the
/// `x`-th from the west and `y`-th from the north.
#[tauri::command(async)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn minimap_image(state: State<'_, AppState>, map: i64, level: u32, x: u32, y: u32) -> CmdResult<tauri::ipc::Response> {
    let m = minimap(state.edition()).ok_or("no minimap")?;
    let &(offset, len) = m
        .tiles
        .get(&(map, level, x, y))
        .ok_or_else(|| format!("no minimap image {map}/{level}/{x}/{y}"))?;
    let bytes = m.images.get(offset..offset + len).ok_or("minimap pack truncated")?;
    Ok(tauri::ipc::Response::new(bytes.to_vec()))
}

/// WebKitGTK renders a blank window under WSLg (no usable GPU for DMA-BUF/EGL),
/// and its DMA-BUF renderer dies with "Error 71 (Protocol error)" on Wayland with
/// the NVIDIA driver: fall back to its software paths in those cases.
#[expect(
    unsafe_code,
    reason = "set_var is unsafe in edition 2024; this runs before any thread is spawned"
)]
fn linux_rendering_workaround() {
    let in_wsl = std::fs::read_to_string("/proc/version").is_ok_and(|v| v.to_lowercase().contains("microsoft"));
    let vars: &[&str] = if in_wsl {
        &["WEBKIT_DISABLE_DMABUF_RENDERER", "WEBKIT_DISABLE_COMPOSITING_MODE"]
    } else if Path::new("/proc/driver/nvidia").exists() {
        &["WEBKIT_DISABLE_DMABUF_RENDERER"]
    } else {
        &[]
    };
    for var in vars {
        if std::env::var_os(var).is_none() {
            // SAFETY: called at startup, before any other thread exists.
            unsafe { std::env::set_var(var, "1") };
        }
    }
}

fn main() {
    if cfg!(target_os = "linux") {
        linux_rendering_workaround();
    }
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            #[cfg(target_os = "macos")]
            {
                let menu = tauri::menu::Menu::default(app.handle())?;
                // Tauri's default macOS menu begins with the application submenu.
                if let Some(app_menu) = menu.items()?.first().and_then(tauri::menu::MenuItemKind::as_submenu) {
                    let check = tauri::menu::MenuItem::with_id(
                        app,
                        "check-for-updates",
                        "Check for Updates…",
                        true,
                        None::<&str>,
                    )?;
                    app_menu.insert(&check, 1)?;
                }
                app.set_menu(menu)?;
                app.on_menu_event(|app, event| {
                    if event.id().as_ref() == "check-for-updates"
                        && let Some(window) = app.get_webview_window("main")
                    {
                        let _ = window.show();
                        let _ = window.unminimize();
                        let _ = window.set_focus();
                        let _ = window.emit("check-for-updates", ());
                    }
                });
            }
            migrate_app_dirs(app.handle());
            let config_file = app.path().app_config_dir()?.join("settings.json");
            let data_dir = app.path().app_data_dir()?;
            let config: Option<serde_json::Value> = std::fs::read_to_string(&config_file)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok());
            if let Some(config) = &config {
                migrate_workspace(config, &data_dir);
            }
            let first_launch = config.is_none();
            let mut settings: Settings = config.and_then(|c| serde_json::from_value(c).ok()).unwrap_or_default();
            // A version's folder chosen by older versions of the app: the game's folder above.
            settings.wow_dir = game_root(&settings.wow_dir);
            if first_launch {
                find_game(&mut settings);
                // Found once: not looked for again at every launch.
                let _ = save_settings_file(&config_file, &settings);
            }
            let db_dir = app.path().app_local_data_dir()?;
            let (auto_wake, wake) = std::sync::mpsc::channel();
            app.manage(AppState {
                settings: Mutex::new(settings),
                restart_needed: Mutex::new(false),
                config_file,
                db_dir,
                dbs: Mutex::new(HashMap::default()),
                names: Mutex::new(HashMap::default()),
                data_dir,
                plan_interrupt: job::Interrupt::default(),
                walks: Mutex::new(HashMap::default()),
                editor: Mutex::new(None),
                edit_requests: job::Requests::default(),
                timeline: Mutex::new(None),
                send_lock: Mutex::new(()),
                auto_wake,
            });
            migrate_guides(&app.state::<AppState>().data_dir);
            migrate_results(&app.state::<AppState>());
            let handle = app.handle().clone();
            std::thread::spawn(move || auto_contribute_loop(&handle, &wake));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            updates_enabled,
            get_settings,
            set_settings,
            get_options,
            plan_route,
            stop_plan,
            cancel_plan,
            export_request,
            install_addon,
            addon_synced,
            polestar_installed,
            export_polestar,
            open_link,
            wow_status,
            detect_wow,
            setup_report,
            addon_health,
            list_editions,
            set_edition,
            contribution_preview,
            send_contribution,
            send_issue,
            need_help,
            set_auto_contribute,
            list_guides,
            get_guide,
            get_version,
            compare_versions,
            set_installed,
            delete_version,
            delete_guide,
            guide_text,
            install_polestar,
            forget_character,
            export_version,
            import_guide,
            map_index,
            map_image,
            minimap_index,
            minimap_image,
            list_characters,
            guide_timeline,
            edit_open,
            edit_simulate,
            edit_suggest,
            edit_save,
            edit_close,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Factoruide");
}

/// At the first launch: the game where it was found, and a game version installed there (WoW
/// Forever first) when the default one is not.
fn find_game(settings: &mut Settings) {
    if matches!(
        game::status(&settings.wow_dir, settings.edition).problem,
        Some(WowProblem::Missing | WowProblem::NoGame)
    ) && let Some(found) = game::detect().into_iter().next()
    {
        settings.wow_dir = found;
    }
    let installed = |e: Edition| e.installed_in(&settings.wow_dir);
    if !installed(settings.edition)
        && let Some(e) = Edition::ALL.iter().copied().find(|e| installed(*e))
    {
        settings.edition = e;
    }
}

/// Switch the edition the app works on (saved with the settings).
#[tauri::command]
fn set_edition(state: State<'_, AppState>, edition: Edition) -> CmdResult<()> {
    let mut settings = state.settings();
    settings.edition = edition;
    set_settings(state, settings)
}

/// Guides made before editions existed (`guides/<slug>/`) are WoW Forever guides.
/// The app was called BrouzQuest (identifier fr.brouz.brouzquest): move its settings, guides and
/// databases to the new identifier's folders, unless these already exist.
fn migrate_app_dirs(app: &AppHandle) {
    let path = app.path();
    for dir in [path.app_config_dir(), path.app_data_dir(), path.app_local_data_dir()]
        .into_iter()
        .flatten()
    {
        let Some(old) = dir.parent().map(|p| p.join("fr.brouz.brouzquest")) else {
            continue;
        };
        if old.is_dir() && !dir.exists() {
            let _ = std::fs::rename(&old, &dir);
        }
    }
}

fn migrate_guides(data_dir: &Path) {
    let root = data_dir.join("guides");
    let Ok(dir) = std::fs::read_dir(&root) else { return };
    for entry in dir.flatten() {
        if entry.path().join("guide.json").is_file() {
            let dest = root.join(Edition::Forever.key());
            let _ = std::fs::create_dir_all(&dest);
            let _ = std::fs::rename(entry.path(), dest.join(entry.file_name()));
        }
    }
}

// Contributions --------------------------------------------------------------------

/// What was sent from this computer for one game version, kept in the app data folder
/// (`contribution-state-<edition>.json`). Its `state` holds character names (never sent) and
/// changes only once the server accepted a contribution.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct SendRecord {
    /// When the server last accepted one (unix seconds).
    #[serde(default)]
    last_sent: Option<u64>,
    /// No send before then (unix seconds): the server asked to wait.
    #[serde(default)]
    retry_after: Option<u64>,
    /// How the last automatic send went.
    #[serde(default)]
    last_auto: Option<AutoSend>,
    /// The last accepted send left new data for a next document (one holds only so much).
    #[serde(default)]
    more: bool,
    #[serde(default)]
    state: SendState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct AutoSend {
    /// Unix seconds.
    time: u64,
    /// "accepted", "already_received", "rate_limited" or "error" (said by `message`).
    status: String,
    #[serde(default)]
    message: Option<String>,
}

fn send_record_file(data_dir: &Path, edition: Edition) -> PathBuf {
    data_dir.join(format!("contribution-state-{}.json", edition.key()))
}

/// The record of a game version, empty when there is none yet. One that cannot be read is an
/// error, not a fresh start: that would send everything again under new character ids.
fn read_send_record(file: &Path) -> CmdResult<SendRecord> {
    match std::fs::read(file) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", file.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(SendRecord::default()),
        Err(e) => Err(format!("{}: {e}", file.display())),
    }
}

/// Write a file whole or not at all: a temporary file renamed over it.
fn write_atomic(file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = file.as_os_str().to_owned();
    tmp.push(".tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, file)
}

fn write_send_record(file: &Path, record: &SendRecord) -> CmdResult<()> {
    write_atomic(file, &serde_json::to_vec_pretty(record).map_err(err)?).map_err(err)
}

/// Sent.lua: the last event of each character the server accepted, for the addon to drop the
/// events up to it from its recordings (by name: a name found in several accounts is left out).
fn sent_lua(state: &SendState) -> String {
    let mut lua = String::from(
        "-- Generated by the Factoruide app. Do not edit.\n\
         -- Last event of each character the contribution server accepted: the addon drops the events up to it.\n\
         FactoruideSent = {\n",
    );
    for (character, last_seq) in state.sent_by_name() {
        let _ = writeln!(lua, "  [{}] = {last_seq},", lua_string(&character));
    }
    lua.push_str("}\n");
    lua
}

/// What a contribution of this computer would hold now, for the current game version.
#[derive(Serialize)]
struct ContributionPreview {
    edition: &'static str,
    /// The game client folder read.
    client_dir: String,
    found: bool,
    /// What is new since the last contribution the server accepted.
    quests: usize,
    /// Creatures and game objects from the client's caches.
    entities: usize,
    events: usize,
    characters: usize,
    locales: Vec<String>,
    /// Size of the JSON sent (bytes).
    size: usize,
    /// What is new does not fit in one document: the rest goes in the next ones.
    more: bool,
    url: Option<String>,
    /// When the server last accepted one of this game version (unix seconds).
    last_sent: Option<u64>,
    /// No send before then (unix seconds): the server asked to wait.
    retry_after: Option<u64>,
    /// The last automatic send of each game version.
    automatic: Vec<AutoStatus>,
}

#[derive(Serialize)]
struct AutoStatus {
    edition: &'static str,
    name: &'static str,
    #[serde(flatten)]
    send: AutoSend,
}

/// The contribution server the app was built with (`FG_CONTRIBUTION_SERVER` at build time).
const DEFAULT_CONTRIBUTION_SERVER: Option<&str> = option_env!("FG_CONTRIBUTION_SERVER");

/// Where contributions are posted, from the configured (or built-in) server address.
fn contribution_endpoint(settings: &Settings) -> Option<String> {
    server_endpoint(settings, "contributions")
}

/// `/v1/<path>` on the contribution server.
fn server_endpoint(settings: &Settings, path: &str) -> Option<String> {
    let base = settings
        .contribution_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .or(DEFAULT_CONTRIBUTION_SERVER)?;
    // Addresses configured for the first server included its path.
    let base = base
        .trim_end_matches('/')
        .trim_end_matches("/v1/contributions")
        .trim_end_matches("/contributions");
    Some(format!("{base}/v1/{path}"))
}

/// This install's contributor id, created the first time (settings locked meanwhile: the
/// automatic sends may ask for it while the settings page saves).
fn contributor_id(state: &AppState) -> CmdResult<String> {
    let mut settings = state.settings.lock().unwrap();
    if let Some(id) = settings
        .contributor
        .clone()
        .filter(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Ok(id);
    }
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(err)?;
    let id = bytes.iter().fold(String::with_capacity(32), |mut id, b| {
        let _ = write!(id, "{b:02x}");
        id
    });
    settings.contributor = Some(id.clone());
    save_settings_file(&state.config_file, &settings)?;
    Ok(id)
}

/// What is new in a game version's client folder since `sent` (one document of it), the
/// state to keep once the server accepted it, whether more is left for a next document, and
/// the folder read.
fn collect_contribution(
    state: &AppState,
    edition: Edition,
    sent: &SendState,
) -> CmdResult<(Contribution, SendState, bool, PathBuf)> {
    let contributor = contributor_id(state)?;
    let client_dir = edition.client_dir(&state.settings().wow_dir);
    let (c, next, more) = fg_contrib::contribution::collect(
        &client_dir,
        edition.key(),
        env!("CARGO_PKG_VERSION"),
        &contributor,
        sent,
    )
    .map_err(err)?;
    Ok((c, next, more, client_dir))
}

// Off the main thread: reading the SavedVariables can take a while.
#[tauri::command]
async fn contribution_preview(app: AppHandle) -> CmdResult<ContributionPreview> {
    tauri::async_runtime::spawn_blocking(move || preview(&app.state::<AppState>()))
        .await
        .map_err(err)?
}

fn preview(state: &AppState) -> CmdResult<ContributionPreview> {
    let settings = state.settings();
    let record = read_send_record(&send_record_file(&state.data_dir, settings.edition))?;
    let (c, _, more, client_dir) = collect_contribution(state, settings.edition, &record.state)?;
    let automatic = Edition::ALL
        .iter()
        .filter_map(|&e| {
            let send = read_send_record(&send_record_file(&state.data_dir, e))
                .ok()?
                .last_auto?;
            Some(AutoStatus {
                edition: e.key(),
                name: e.rules().edition_name,
                send,
            })
        })
        .collect();
    Ok(ContributionPreview {
        edition: settings.edition.key(),
        found: client_dir.is_dir(),
        client_dir: client_dir.display().to_string(),
        quests: c.quests(),
        entities: c.entities(),
        events: c.events(),
        characters: c.collector.len(),
        locales: c.quest_cache.iter().map(|f| f.locale.clone()).collect(),
        size: serde_json::to_vec(&c).map_err(err)?.len(),
        more,
        url: contribution_endpoint(&settings),
        last_sent: record.last_sent,
        retry_after: record.retry_after.filter(|&t| now().is_ok_and(|now| now < t)),
        automatic,
    })
}

/// The server's answer to a contribution.
#[derive(Debug, Clone, PartialEq)]
enum Posted {
    /// 201: kept.
    Accepted,
    /// 200: the server already had this document.
    AlreadyReceived,
    /// 429: too many sends; wait that many seconds when the server says.
    RateLimited(Option<u64>),
    Failed(String),
}

impl Posted {
    fn accepted(&self) -> bool {
        matches!(self, Posted::Accepted | Posted::AlreadyReceived)
    }
    fn status(&self) -> &'static str {
        match self {
            Posted::Accepted => "accepted",
            Posted::AlreadyReceived => "already_received",
            Posted::RateLimited(_) => "rate_limited",
            Posted::Failed(_) => "error",
        }
    }
}

/// Longest wait asked by the server that is honored (a week).
const MAX_RETRY_AFTER: u64 = 7 * 86_400;

/// The seconds of a "Retry-After" header (its date form is not read: the next check waits).
fn retry_after_secs(value: &str) -> Option<u64> {
    value.trim().parse::<u64>().ok().map(|s| s.min(MAX_RETRY_AFTER))
}

/// Post a contribution document (or a report). Nothing about this computer goes with it: a fixed user agent,
/// no cookie, no proxy from the environment, and no redirect followed (it could take the
/// document to another host).
fn post_contribution(url: &str, body: &[u8]) -> Posted {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .max_redirects(0)
        .proxy(None)
        .timeout_connect(Some(std::time::Duration::from_secs(20)))
        .timeout_global(Some(std::time::Duration::from_secs(120)))
        .user_agent("Factoruide")
        // Error answers are read below (their message, a 429's Retry-After).
        .http_status_as_error(false)
        .build()
        .into();
    let mut r = match agent.post(url).header("Content-Type", "application/json").send(body) {
        Ok(r) => r,
        Err(e) => return Posted::Failed(err(e)),
    };
    match r.status().as_u16() {
        201 => Posted::Accepted,
        200 => Posted::AlreadyReceived,
        429 => Posted::RateLimited(
            r.headers()
                .get("Retry-After")
                .and_then(|v| v.to_str().ok())
                .and_then(retry_after_secs),
        ),
        code @ 400.. => {
            let json: serde_json::Value = r.body_mut().read_json().unwrap_or_default();
            let message = json["error"].as_str().or(json["status"].as_str()).unwrap_or("");
            Posted::Failed(format!("{code}: {message}"))
        }
        code => Posted::Failed(format!("{code}: unexpected answer")),
    }
}

/// A contribution ready to post, with the record of its game version it updates.
struct Outgoing {
    file: PathBuf,
    record: SendRecord,
    contribution: Contribution,
    /// The state to keep once the server accepted it.
    next: SendState,
    /// New data is left for a next document.
    more: bool,
}

/// Post a contribution with `post` and record the answer: the new state is kept only when the
/// server accepted the document, so what failed goes again next time.
fn deliver(
    out: Outgoing,
    now: u64,
    automatic: bool,
    post: impl FnOnce(&[u8]) -> Posted,
) -> CmdResult<(Posted, SendRecord)> {
    let Outgoing {
        file,
        mut record,
        contribution,
        next,
        more,
    } = out;
    let posted = post(&serde_json::to_vec(&contribution).map_err(err)?);
    match &posted {
        Posted::Accepted | Posted::AlreadyReceived => {
            record.state = next;
            record.last_sent = Some(now);
            record.retry_after = None;
            record.more = more;
        }
        Posted::RateLimited(wait) => record.retry_after = wait.map(|w| now + w),
        Posted::Failed(_) => {}
    }
    if automatic {
        record.last_auto = Some(AutoSend {
            time: now,
            status: posted.status().to_owned(),
            message: match &posted {
                Posted::Failed(m) => Some(m.clone()),
                _ => None,
            },
        });
    }
    write_send_record(&file, &record)?;
    Ok((posted, record))
}

/// Send a contribution to the server (the caller holds `send_lock`) and, once accepted, tell
/// the addon what it can drop.
fn send(state: &AppState, url: &str, out: Outgoing, now: u64, automatic: bool) -> CmdResult<(Posted, SendRecord)> {
    let (posted, record) = deliver(out, now, automatic, |body| post_contribution(url, body))?;
    if posted.accepted() {
        // The data addon tells the addon what it can drop (when the guides were sent to the game
        // already: never sends them by itself); a failure only delays the purge.
        let settings = state.settings();
        if settings.addons_dir(settings.edition).join(DATA_ADDON).exists() {
            let _ = write_data(state);
        }
    }
    Ok((posted, record))
}

/// Documents sent in one click at most (the server takes a short burst).
const MANUAL_BURST: usize = 3;

/// How a burst of documents went.
#[derive(Debug, Default, PartialEq)]
struct Burst {
    /// Documents the server accepted, and the answer to the last of them.
    accepted: usize,
    last_accepted: Option<Posted>,
    /// The answer that ended the burst when it was not an acceptance.
    refused: Option<Posted>,
    /// New data is left (not sent yet).
    more: bool,
}

/// Send documents one after the other while the server accepts them and more is left, at most
/// `MANUAL_BURST`. `send_one` sends the next one: its answer and whether more is left, None
/// when there is nothing to send.
fn send_burst(mut send_one: impl FnMut() -> CmdResult<Option<(Posted, bool)>>) -> CmdResult<Burst> {
    let mut burst = Burst::default();
    for _ in 0..MANUAL_BURST {
        let Some((posted, more)) = send_one()? else {
            break;
        };
        if !posted.accepted() {
            burst.refused = Some(posted);
            burst.more = true;
            break;
        }
        burst.accepted += 1;
        burst.last_accepted = Some(posted);
        burst.more = more;
        if !more {
            break;
        }
    }
    Ok(burst)
}

/// What a click on "send" did.
#[derive(Serialize)]
struct SendReport {
    /// "accepted", or "already_received" when the server already had the last document.
    status: &'static str,
    /// Documents the server accepted.
    documents: usize,
    /// New data is left for the next send.
    more: bool,
}

/// Send what is new of the current game version, in several documents when it does not fit in
/// one (`MANUAL_BURST` at most). Errors, when nothing was accepted: "nothing_to_send",
/// "rate_limited", or the server's message.
#[tauri::command]
async fn send_contribution(app: AppHandle) -> CmdResult<SendReport> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let _sending = state
            .send_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let edition = state.edition();
        let url = contribution_endpoint(&state.settings()).ok_or("no contribution server configured")?;
        let file = send_record_file(&state.data_dir, edition);
        let mut record = read_send_record(&file)?;
        if record.retry_after.is_some_and(|t| now().is_ok_and(|now| now < t)) {
            return Err("rate_limited".to_owned());
        }
        let burst = send_burst(|| {
            let (contribution, next, more, _) = collect_contribution(&state, edition, &record.state)?;
            if contribution.is_empty() {
                return Ok(None);
            }
            let out = Outgoing {
                file: file.clone(),
                record: record.clone(),
                contribution,
                next,
                more,
            };
            let (posted, sent) = send(&state, &url, out, now()?, false)?;
            record = sent;
            Ok(Some((posted, more)))
        })?;
        match (burst.last_accepted, burst.refused) {
            (Some(last), _) => Ok(SendReport {
                status: last.status(),
                documents: burst.accepted,
                more: burst.more,
            }),
            (None, Some(Posted::RateLimited(_))) => Err("rate_limited".to_owned()),
            (None, Some(Posted::Failed(message))) => Err(message),
            (None, _) => Err("nothing_to_send".to_owned()),
        }
    })
    .await
    .map_err(err)?
}

/// A problem the player reports, as the report form gives it.
#[derive(Deserialize)]
struct IssueDraft {
    /// `guide` or `other`.
    kind: String,
    message: String,
    #[serde(default)]
    name: Option<String>,
    /// The app's language.
    language: String,
    #[serde(default)]
    guide: Option<IssueGuide>,
    /// Join how the game and the addon are set up (`setup_report`).
    #[serde(default)]
    setup: bool,
}

#[derive(Deserialize)]
struct IssueGuide {
    guide: String,
    version: u32,
    /// Indices of the steps pointed at.
    #[serde(default)]
    steps: Vec<usize>,
    /// Language the steps were shown in.
    locale: String,
}

/// What a report says of a guide: everything it was computed from, so that it is computed again
/// exactly (all its settings, seed included), without the character's name.
fn guide_report(state: &AppState, g: &IssueGuide) -> CmdResult<(Edition, fg_contrib::issue::GuideReport)> {
    let v = state.version(&g.guide, g.version)?;
    let mut request = v.request;
    let edition = request.params.as_ref().map_or_else(|| state.edition(), |p| p.edition);
    if request.params.is_none() {
        request.params = Some(overrides(edition)?.params);
    }
    request.name = None;
    if let Some(start) = &mut request.start {
        start.name.clear();
    }
    let route = state.render(&v.outcome.route, Some(&g.locale))?;
    let mut steps = Vec::new();
    for &i in &g.steps {
        let step = serde_json::to_value(route.steps.get(i).ok_or("no such step")?).map_err(err)?;
        let map = &step["map"];
        steps.push(fg_contrib::issue::ReportedStep {
            index: i as u32,
            kind: step["kind"].as_str().unwrap_or_default().to_owned(),
            quest: step["quest"].as_u64().map(|q| q as u32),
            objective: step["objective"].as_u64().map(|o| o as u32),
            text: step["text"].as_str().unwrap_or_default().chars().take(500).collect(),
            zone: step["zone"].as_str().map(str::to_owned),
            level: step["level"].as_u64().unwrap_or_default() as u32,
            map: map["ui_map"]
                .as_u64()
                .zip(map["x"].as_f64().zip(map["y"].as_f64()))
                .map(|(m, (x, y))| (m as u32, x, y)),
        });
    }
    Ok((
        edition,
        fg_contrib::issue::GuideReport {
            request: serde_json::to_value(&request).map_err(err)?,
            plan: serde_json::to_value(&v.outcome.plan).map_err(err)?,
            data: v.meta.data,
            version: g.version,
            edited: v.meta.edited,
            imported: v.meta.imported,
            total_time: v.meta.total_time,
            steps,
        },
    ))
}

/// What players can check in game for the current game version ("Need help" page), with the
/// names in `locale` (the game's language).
#[tauri::command]
async fn need_help(app: AppHandle, locale: String) -> CmdResult<Vec<fg_route::help::Check>> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let edition = state.edition();
        let defs = fg_route::help::load(
            |name| {
                let (_, bytes) = embedded::OVERRIDES.iter().find(|(n, _)| *n == name)?;
                Some(String::from_utf8_lossy(bytes).into_owned())
            },
            edition,
        )
        .map_err(err)?;
        fg_route::help::resolve(&state.open_db()?, &defs, edition, &locale).map_err(err)
    })
    .await
    .map_err(err)?
}

/// Send a problem the player reports to the contribution server. Errors: "rate_limited", or
/// the reason (the report's check, the server's message).
#[tauri::command]
async fn send_issue(app: AppHandle, draft: IssueDraft) -> CmdResult<&'static str> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let url = server_endpoint(&state.settings(), "issues").ok_or("no contribution server configured")?;
        let (edition, guide) = match &draft.guide {
            Some(g) => {
                let (e, report) = guide_report(&state, g)?;
                (e, Some(report))
            }
            None => (state.edition(), None),
        };
        let now = now()?;
        let issue = fg_contrib::issue::Issue {
            format: fg_contrib::issue::FORMAT.into(),
            version: fg_contrib::issue::VERSION,
            edition: edition.key().into(),
            app_version: env!("CARGO_PKG_VERSION").into(),
            contributor: contributor_id(&state)?,
            day: (now / 86_400) as u32,
            os: std::env::consts::OS.into(),
            language: draft.language,
            kind: draft.kind,
            message: draft.message.trim().to_owned(),
            name: draft.name.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()),
            guide,
            setup: draft.setup.then(|| setup_of(&state)),
        };
        fg_contrib::issue::validate(&issue, now).map_err(err)?;
        let post = |issue: &fg_contrib::issue::Issue| {
            let body = serde_json::to_vec(issue).map_err(err)?;
            if body.len() > fg_contrib::issue::MAX_BYTES {
                return Err("report too large".to_owned());
            }
            Ok(post_contribution(&url, &body))
        };
        let mut posted = post(&issue)?;
        // A server older than the setup part refuses the document: sent again without it.
        if let Posted::Failed(message) = &posted
            && issue.setup.is_some()
            && message.contains("invalid report document")
        {
            posted = post(&fg_contrib::issue::Issue { setup: None, ..issue })?;
        }
        match posted {
            Posted::RateLimited(_) => Err("rate_limited".to_owned()),
            Posted::Failed(message) => Err(message),
            posted => Ok(posted.status()),
        }
    })
    .await
    .map_err(err)?
}

/// A folder as a report shows it: the user's folder name (after Users or home) hidden.
fn private_path(path: &Path) -> String {
    let mut out = Vec::new();
    let mut hide = false;
    for part in path.components() {
        let text = part.as_os_str().to_string_lossy().into_owned();
        out.push(if hide { "~".to_owned() } else { text.clone() });
        hide = text.eq_ignore_ascii_case("users") || text == "home";
    }
    let sep = if cfg!(windows) { "\\" } else { "/" };
    out.join(sep).replace(&format!("{sep}{sep}"), sep)
}

/// A version as reports take it ("0.13.0"; "?" when the game did not tell).
fn version_like(v: &str) -> bool {
    !v.is_empty() && v.len() <= 32 && v.bytes().all(|b| b.is_ascii_alphanumeric() || b"+-.".contains(&b))
}

/// How the game and the addon are set up on this computer, for a report.
fn setup_of(state: &AppState) -> fg_contrib::issue::Setup {
    let settings = state.settings();
    let status = game::status(&settings.wow_dir, settings.edition);
    let polestar = settings.edition == Edition::Forever && polestar_installed_in(&settings);
    let health = game::addon_health(&status.root, settings.edition, ADDON_VERSION);
    let today = now().unwrap_or_default().cast_signed();
    fg_contrib::issue::Setup {
        wow_dir: private_path(&status.root).chars().take(300).collect(),
        clients: status.clients.into_iter().take(20).collect(),
        problem: status.problem.map(|p| p.key().to_owned()),
        guide_addon: if polestar { "polestar" } else { "factoruide" }.into(),
        addon_version: health.installed_version.filter(|v| version_like(v)),
        addon_complete: health.complete,
        seen_days_ago: health
            .seen
            .as_ref()
            .and_then(|s| u32::try_from((today - s.time).max(0) / 86_400).ok()),
        seen_version: health.seen.map(|s| s.version).filter(|v| version_like(v)),
        not_loaded: health.not_loaded,
        disabled: u32::try_from(health.disabled_for.len()).unwrap_or(u32::MAX),
        saved_variables: health.saved_variables,
        restart_needed: *state.restart_needed.lock().unwrap(),
    }
}

/// What a report would join about the game and the addon (shown before it is sent).
#[tauri::command(async)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri commands take their arguments by value"
)]
fn setup_report(state: State<'_, AppState>) -> fg_contrib::issue::Setup {
    setup_of(&state)
}

/// First automatic check after startup.
const AUTO_FIRST_CHECK: std::time::Duration = std::time::Duration::from_secs(120);
/// Seconds between two automatic checks.
const AUTO_CHECK_EVERY: u64 = 6 * 3600;
/// An automatic send at most this often per game version (seconds).
const AUTO_SEND_EVERY: u64 = 24 * 3600;
/// When the last send left new data for a next document: the next one after this long
/// (seconds), so that a backlog goes in a few sends.
const AUTO_BACKLOG_EVERY: u64 = 3600;

/// Whether to send a game version's new data on its own now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoDecision {
    Off,
    /// The server asked to wait.
    BackingOff,
    /// Sent less than `AUTO_SEND_EVERY` ago (`AUTO_BACKLOG_EVERY` when it left more).
    NotDue,
    NothingNew,
    Send,
}

/// `has_new` reads the game folder: asked last, when everything else says to send.
fn auto_decision(enabled: bool, record: &SendRecord, now: u64, has_new: impl FnOnce() -> bool) -> AutoDecision {
    if !enabled {
        AutoDecision::Off
    } else if record.retry_after.is_some_and(|t| now < t) {
        AutoDecision::BackingOff
    } else if record
        .last_sent
        .is_some_and(|t| now.saturating_sub(t) < auto_every(record))
    {
        AutoDecision::NotDue
    } else if !has_new() {
        AutoDecision::NothingNew
    } else {
        AutoDecision::Send
    }
}

fn auto_every(record: &SendRecord) -> u64 {
    if record.more {
        AUTO_BACKLOG_EVERY
    } else {
        AUTO_SEND_EVERY
    }
}

/// Seconds until a game version wants a check before the regular one: the end of the server's
/// wait, or the next document of a backlog. Only times to come count: one past and not acted
/// on (sends off, an error) waits for the regular check.
fn auto_due_in(record: &SendRecord, now: u64) -> Option<u64> {
    let backlog = record.last_sent.filter(|_| record.more).map(|t| t + AUTO_BACKLOG_EVERY);
    [record.retry_after, backlog]
        .into_iter()
        .flatten()
        .filter(|&t| t > now)
        .map(|t| t - now)
        .min()
}

/// The automatic contributions: a check shortly after startup, then every `AUTO_CHECK_EVERY`
/// (sooner when the server's wait ends first, or when they are turned on).
fn auto_contribute_loop(app: &AppHandle, wake: &std::sync::mpsc::Receiver<()>) {
    let mut wait = AUTO_FIRST_CHECK;
    loop {
        if let Err(std::sync::mpsc::RecvTimeoutError::Disconnected) = wake.recv_timeout(wait) {
            return;
        }
        wait = auto_contribute(&app.state::<AppState>());
    }
}

/// Send what is new of every game version whose client is installed, when due (outcomes kept
/// in their records, shown on the Contribute page). Returns the time until the next check.
fn auto_contribute(state: &AppState) -> std::time::Duration {
    let mut next = AUTO_CHECK_EVERY;
    let settings = state.settings();
    let (Some(url), Ok(now)) = (contribution_endpoint(&settings), now()) else {
        return std::time::Duration::from_secs(next);
    };
    for edition in Edition::ALL {
        if !edition.installed_in(&settings.wow_dir) {
            continue;
        }
        let _sending = state
            .send_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let file = send_record_file(&state.data_dir, edition);
        // Unreadable: left alone, never overwritten.
        let Ok(mut record) = read_send_record(&file) else {
            continue;
        };
        let mut collected = Ok(None);
        let decision = auto_decision(settings.auto_contribute, &record, now, || {
            match collect_contribution(state, edition, &record.state) {
                Ok((c, next, more, _)) => {
                    let new = !c.is_empty();
                    collected = Ok(Some((c, next, more)));
                    new
                }
                Err(e) => {
                    collected = Err(e);
                    false
                }
            }
        });
        match (decision, collected) {
            (AutoDecision::Send, Ok(Some((contribution, next_state, more)))) => {
                let out = Outgoing {
                    file,
                    record,
                    contribution,
                    next: next_state,
                    more,
                };
                match send(state, &url, out, now, true) {
                    Ok((_, sent)) => record = sent,
                    Err(_) => continue,
                }
            }
            (_, Err(e)) => {
                record.last_auto = Some(AutoSend {
                    time: now,
                    status: "error".to_owned(),
                    message: Some(e),
                });
                let _ = write_send_record(&file, &record);
            }
            _ => {}
        }
        if let Some(due) = auto_due_in(&record, now) {
            next = next.min(due);
        }
    }
    std::time::Duration::from_secs(next.max(60))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A character taken out stays out until it is played again (a newer profile), on its game
    /// version only.
    #[test]
    fn a_forgotten_character_is_back_when_played_again() {
        let profile = |time| StartState {
            name: "Finnagnix-ClassicBetaPvP".into(),
            time,
            ..StartState::default()
        };
        let mut forgotten = BTreeMap::new();
        forgotten.insert(forgotten_key(Edition::Forever, "Finnagnix-ClassicBetaPvP"), 100);
        assert!(is_forgotten(&forgotten, Edition::Forever, &profile(100)));
        assert!(!is_forgotten(&forgotten, Edition::Forever, &profile(101)));
        assert!(!is_forgotten(&forgotten, Edition::Classic, &profile(100)));
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fg-app-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sent(characters: &[(&str, i64)]) -> SendState {
        SendState {
            characters: characters
                .iter()
                .map(|&(name, last_seq)| {
                    let id = format!("{last_seq:016x}");
                    (
                        name.to_owned(),
                        fg_contrib::contribution::SentCharacter { id, last_seq },
                    )
                })
                .collect(),
            ..SendState::default()
        }
    }

    /// Character names come back from Sent.lua as they are, whatever they hold; a name found in
    /// two accounts is left out (the addon could not tell their events apart).
    #[test]
    fn sent_lua_gives_the_addon_every_name_as_is() {
        let names = [
            ("Brouz-Archimonde", 12),
            ("Élodie-Kirin Tor", 3),
            ("Odd\"name\\-Realm", 7),
            ("Line\nbreak\r\u{0}1-Realm", 9),
        ];
        let keys: Vec<(String, i64)> = names.iter().map(|&(n, s)| (format!("ACCOUNT1/{n}"), s)).collect();
        let mut keys: Vec<(&str, i64)> = keys.iter().map(|(k, s)| (k.as_str(), *s)).collect();
        keys.extend([("ACCOUNT1/Twin-Realm", 4), ("ACCOUNT2/Twin-Realm", 8)]);
        let dir = temp_dir("sent-lua");
        std::fs::write(dir.join("Sent.lua"), sent_lua(&sent(&keys).clone())).unwrap();
        let lua = fg_contrib::lua::new_lua();
        let table = fg_contrib::lua::load_globals(&lua, &dir.join("Sent.lua"), &["FactoruideSent"])
            .unwrap()
            .pop()
            .unwrap();
        let mut read: Vec<(String, i64)> = table
            .str_entries()
            .into_iter()
            .map(|(name, seq)| (name.to_owned(), seq.int().unwrap()))
            .collect();
        read.sort();
        let mut expected: Vec<(String, i64)> = names.iter().map(|&(n, s)| (n.to_owned(), s)).collect();
        expected.sort();
        assert_eq!(read, expected);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The data addon's text reads back as it was, whatever brackets the guides hold.
    #[test]
    fn the_data_addon_holds_the_text_as_is() {
        let text = "FG.Routes[\"a]]b\"] = { t = \"x]=]y\" }\n";
        let dir = temp_dir("data-lua");
        std::fs::write(
            dir.join("Data.lua"),
            format!("FactoruideData = {}\n", job::lua_long_string(text)),
        )
        .unwrap();
        let lua = fg_contrib::lua::new_lua();
        let read = fg_contrib::lua::load_globals(&lua, &dir.join("Data.lua"), &["FactoruideData"])
            .unwrap()
            .pop()
            .unwrap();
        // A long string drops the line break right after its opening.
        assert_eq!(read.str(), Some(text));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn outgoing(file: &Path, record: &SendRecord, next: SendState, more: bool) -> Outgoing {
        Outgoing {
            file: file.to_owned(),
            record: record.clone(),
            contribution: Contribution {
                format: fg_contrib::contribution::FORMAT.into(),
                version: fg_contrib::contribution::VERSION,
                edition: "classic".into(),
                app_version: "0.0.0".into(),
                contributor: "0".repeat(32),
                day: 20_000,
                quest_cache: Vec::new(),
                creature_cache: Vec::new(),
                object_cache: Vec::new(),
                collector: Vec::new(),
            },
            next,
            more,
        }
    }

    /// What was sent is only noted once the server accepted it: a failed or refused send goes
    /// again, whole, next time.
    #[test]
    fn the_state_is_kept_only_once_the_server_accepted() {
        let dir = temp_dir("deliver");
        let file = send_record_file(&dir, Edition::Classic);
        let before = SendRecord {
            last_sent: Some(1_000),
            state: sent(&[("Brouz-Archimonde", 5)]),
            ..SendRecord::default()
        };
        write_send_record(&file, &before).unwrap();
        let next = sent(&[("Brouz-Archimonde", 9)]);
        let now = 100_000;

        deliver(outgoing(&file, &before, next.clone(), true), now, false, |_| {
            Posted::Failed("500: oops".into())
        })
        .unwrap();
        assert_eq!(read_send_record(&file).unwrap(), before);

        deliver(outgoing(&file, &before, next.clone(), true), now, true, |_| {
            Posted::RateLimited(Some(600))
        })
        .unwrap();
        let record = read_send_record(&file).unwrap();
        assert_eq!(record.state, before.state);
        assert_eq!(record.last_sent, Some(1_000));
        assert_eq!(record.retry_after, Some(now + 600));
        assert_eq!(record.last_auto.unwrap().status, "rate_limited");

        for answer in [Posted::Accepted, Posted::AlreadyReceived] {
            deliver(outgoing(&file, &before, next.clone(), true), now, false, |body| {
                assert!(serde_json::from_slice::<Contribution>(body).is_ok());
                answer
            })
            .unwrap();
            let record = read_send_record(&file).unwrap();
            assert_eq!(record.state, next);
            assert_eq!(record.last_sent, Some(now));
            assert_eq!(record.retry_after, None);
            assert!(record.more);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A record that cannot be read stops the sends instead of starting over (new ids).
    #[test]
    fn an_unreadable_record_is_an_error_a_missing_one_is_empty() {
        let dir = temp_dir("record");
        let file = send_record_file(&dir, Edition::Tbc);
        assert_eq!(read_send_record(&file).unwrap(), SendRecord::default());
        std::fs::write(&file, "{ not json").unwrap();
        assert!(read_send_record(&file).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn automatic_sends_once_a_day_when_there_is_something_new() {
        let now = 10_000_000;
        let never = SendRecord::default();
        let sent_at = |t| SendRecord {
            last_sent: Some(t),
            ..SendRecord::default()
        };
        let unasked = || -> bool { panic!("the game folder is read only when a send is due") };
        assert_eq!(auto_decision(false, &never, now, unasked), AutoDecision::Off);
        assert_eq!(auto_decision(true, &never, now, || true), AutoDecision::Send);
        assert_eq!(auto_decision(true, &never, now, || false), AutoDecision::NothingNew);
        assert_eq!(
            auto_decision(true, &sent_at(now - 3600), now, unasked),
            AutoDecision::NotDue
        );
        assert_eq!(
            auto_decision(true, &sent_at(now - AUTO_SEND_EVERY - 1), now, || true),
            AutoDecision::Send
        );
        let waiting = SendRecord {
            retry_after: Some(now + 60),
            ..SendRecord::default()
        };
        assert_eq!(auto_decision(true, &waiting, now, unasked), AutoDecision::BackingOff);
        let waited = SendRecord {
            retry_after: Some(now - 60),
            ..SendRecord::default()
        };
        assert_eq!(auto_decision(true, &waited, now, || true), AutoDecision::Send);
    }

    /// A send that left more is followed by the next document an hour later, not a day.
    #[test]
    fn a_backlog_goes_hourly_and_the_server_wait_still_holds() {
        let now = 10_000_000;
        let backlog = |sent| SendRecord {
            last_sent: Some(sent),
            more: true,
            ..SendRecord::default()
        };
        let unasked = || -> bool { panic!("not due") };
        assert_eq!(
            auto_decision(true, &backlog(now - 600), now, unasked),
            AutoDecision::NotDue
        );
        assert_eq!(
            auto_decision(true, &backlog(now - AUTO_BACKLOG_EVERY), now, || true),
            AutoDecision::Send
        );
        let waiting = SendRecord {
            retry_after: Some(now + 7200),
            ..backlog(now - AUTO_BACKLOG_EVERY)
        };
        assert_eq!(auto_decision(true, &waiting, now, unasked), AutoDecision::BackingOff);

        // The next check: when the backlog's next document is due, or the server's wait ends.
        assert_eq!(auto_due_in(&backlog(now), now), Some(AUTO_BACKLOG_EVERY));
        assert_eq!(auto_due_in(&waiting, now), Some(7200));
        let both = SendRecord {
            retry_after: Some(now + 60),
            ..backlog(now)
        };
        assert_eq!(auto_due_in(&both, now), Some(60));
        // Nothing left, or a due time already past (sends off): the regular check.
        assert_eq!(
            auto_due_in(
                &SendRecord {
                    more: false,
                    ..backlog(now)
                },
                now
            ),
            None
        );
        assert_eq!(auto_due_in(&backlog(now - 2 * AUTO_BACKLOG_EVERY), now), None);
    }

    /// One click sends documents while the server accepts them and more is left, three at most.
    #[test]
    fn a_click_sends_a_backlog_in_a_short_burst() {
        let run = |answers: Vec<Option<(Posted, bool)>>| {
            let mut answers = answers.into_iter();
            let mut asked = 0;
            let burst = send_burst(|| {
                asked += 1;
                Ok(answers.next().flatten())
            })
            .unwrap();
            (burst, asked)
        };
        let (burst, asked) = run(vec![Some((Posted::Accepted, false))]);
        assert_eq!((burst.accepted, burst.more, asked), (1, false, 1));

        let (burst, asked) = run(vec![
            Some((Posted::Accepted, true)),
            Some((Posted::Accepted, true)),
            Some((Posted::AlreadyReceived, true)),
            Some((Posted::Accepted, true)),
        ]);
        assert_eq!((burst.accepted, burst.more, asked), (3, true, 3));
        assert_eq!(burst.last_accepted, Some(Posted::AlreadyReceived));

        let (burst, asked) = run(vec![
            Some((Posted::Accepted, true)),
            Some((Posted::RateLimited(None), true)),
        ]);
        assert_eq!((burst.accepted, burst.more, asked), (1, true, 2));
        assert_eq!(burst.refused, Some(Posted::RateLimited(None)));

        let (burst, _) = run(vec![Some((Posted::Failed("500: oops".into()), true))]);
        assert_eq!(burst.accepted, 0);
        assert_eq!(burst.refused, Some(Posted::Failed("500: oops".into())));

        let (burst, asked) = run(vec![None]);
        assert_eq!((burst, asked), (Burst::default(), 1));
    }

    #[test]
    fn retry_after_in_seconds_is_honored_up_to_a_week() {
        assert_eq!(retry_after_secs(" 120 "), Some(120));
        assert_eq!(retry_after_secs("99999999"), Some(MAX_RETRY_AFTER));
        assert_eq!(retry_after_secs("Wed, 21 Oct 2026 07:28:00 GMT"), None);
    }
}
