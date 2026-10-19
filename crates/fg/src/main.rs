mod audit;
mod diff;
mod route;
mod talents;
mod travel;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use fg_route::xp::Edition;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "fg",
    about = "Factoruide planner: plan leveling routes on the app's quest databases"
)]
struct Cli {
    /// Game version: forever (default), classic, tbc. Sets the database
    /// (data/<edition>.sqlite.zst) and the overrides (overrides/, with overrides/<edition>/ added).
    #[arg(long, env = "FG_EDITION", default_value = "forever", global = true)]
    edition: String,
    /// Normalized database (default: the edition's, unpacked from data/<edition>.sqlite.zst)
    #[arg(long, env = "FG_DB", global = true)]
    db: Option<PathBuf>,
    /// Directory of the planned routes (JSON)
    #[arg(long, env = "FG_ROUTES", default_value = "routes", global = true)]
    routes: PathBuf,
    /// Addon directory receiving Routes.lua
    #[arg(long, env = "FG_ADDON_DIR", default_value = "addon/Factoruide_Guide", global = true)]
    addon_dir: PathBuf,
    #[command(subcommand)]
    command: Command,
}

/// The characters a guide starts from, as their addon recorded them.
#[derive(clap::Args)]
struct Players {
    /// Start from a character as its addon recorded it: a profile JSON (as --profile-out
    /// writes); its level replaces --from-level (--race and --class are its own; in a group,
    /// the guide is planned for the player with the least XP)
    #[arg(long)]
    start: Option<PathBuf>,
    /// Another player of a group guide, a profile JSON (repeatable, needs --start): the
    /// same guide for everyone, every player level 10 or more and of one faction
    #[arg(long = "mate")]
    mates: Vec<PathBuf>,
    /// Write where the guide leaves the character as a profile JSON (a made-up player, to
    /// start other guides from: --start, --mate)
    #[arg(long)]
    profile_out: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Plan a leveling route and export it (routes/*.json and the addon's Routes.lua)
    Route {
        /// Race key from overrides/races.toml (gnome, human, orc...)
        #[arg(long)]
        race: String,
        /// Class (warrior, mage...)
        #[arg(long)]
        class: String,
        #[arg(long, default_value_t = 1)]
        from_level: i64,
        /// Default: the edition's level cap
        #[arg(long)]
        to_level: Option<i64>,
        /// How much the optimization tries (1 = normal, 0.1 = preview, 3 = thorough; 0 = the first
        /// routes only): the same guide on any machine, only slower with fewer processors
        #[arg(long)]
        effort: Option<f64>,
        /// Initial construction: regions (quest groups) or greedy
        #[arg(long)]
        construction: Option<String>,
        /// Route name (default: <race>-<class>)
        #[arg(long)]
        name: Option<String>,
        /// Allow quests with elite objectives
        #[arg(long)]
        allow_elite: bool,
        /// Do not run dungeons
        #[arg(long)]
        no_dungeons: bool,
        /// Battleground (PvP) quests: marks of honor and battleground objectives
        #[arg(long)]
        pvp_quests: bool,
        /// Progression profile: cautious, normal or risky (how far above its power the character goes)
        #[arg(long, value_parser = ["cautious", "normal", "risky"])]
        progression: Option<String>,
        /// Professions leveled along the route (their quests are planned, optional), as key[:target] (tailoring:300, fishing, cooking:225)
        #[arg(long, value_delimiter = ',')]
        profession: Vec<String>,
        /// Planner parameters, as key=value (repeatable)
        #[arg(long = "param")]
        params: Vec<String>,
        /// Language of the guide texts (frFR, deDE...; English where not translated)
        #[arg(long)]
        locale: Option<String>,
        #[command(flatten)]
        players: Box<Players>,
    },
    /// Summarize a planned route (zone changes, or every step with --full)
    Show {
        name: String,
        #[arg(long)]
        full: bool,
    },
    /// Differences between two guides, step by step in their order (added, removed, changed,
    /// moved steps and zones), with the figures and settings that changed
    Diff {
        /// First guide: a version saved by the app (its folder, e.g. .../guides/forever/<guide>/v3,
        /// or its outcome.json), or a route of `fg route` (file or name). Alone, a guide folder of
        /// the app: its two latest versions.
        a: PathBuf,
        /// Second guide (the newer one)
        b: Option<PathBuf>,
        /// The differences as JSON (what the app shows) instead of text
        #[arg(long)]
        json: bool,
    },
    /// Regenerate the addon's Routes.lua from routes/*.json
    Addon,
    /// Write the guide pack for the Polestar addon (WoW Forever) from every route JSON
    Polestar {
        /// The pack's addon folder (e.g. <WoW>/_classic_beta_/Interface/AddOns/Factoruide_Polestar),
        /// with --source the pack's folder for Polestar's own tools
        #[arg(long, default_value = "Factoruide_Polestar")]
        out: PathBuf,
        /// Write the guides' sources instead (pack.json and a .guide file each, in English), for
        /// Polestar's own tools (route planner, playground, compiler)
        #[arg(long)]
        source: bool,
    },
    /// Replay a request exported by the app (debug/request-*.json)
    Replay {
        file: PathBuf,
        /// Override how much the optimization tries (see `route --effort`)
        #[arg(long)]
        effort: Option<f64>,
    },
    /// Walking distance and passages between two points (Zone,x,y)
    Travel {
        from: String,
        to: String,
        #[arg(long, default_value = "Alliance")]
        faction: String,
    },
    /// Look for each known kind of data problem in all the edition's data, for every race
    /// (accepted findings: overrides/audit.toml); fails when some are left to fix
    Audit {
        /// Checks to run (default: all): islands, source-items
        #[arg(long = "check")]
        checks: Vec<String>,
        /// Also list the accepted findings
        #[arg(long)]
        all: bool,
    },
    /// Write the leveling talent plans of the addon (TalentBuilds_<edition>.lua) from the game's talent trees
    Talents {
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

/// The edition's normalized database: `data/<edition>.sqlite`, unpacked from
/// `data/<edition>.sqlite.zst` when missing or older.
fn database(edition: Edition) -> Result<PathBuf> {
    let packed = PathBuf::from(format!("data/{}.sqlite.zst", edition.key()));
    let path = PathBuf::from(format!("data/{}.sqlite", edition.key()));
    let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    if packed.exists() && (!path.exists() || modified(&packed) > modified(&path)) {
        eprintln!("unpacking {}", packed.display());
        let data = zstd::decode_all(std::fs::File::open(&packed)?)?;
        std::fs::write(&path, data)?;
    }
    anyhow::ensure!(
        path.exists(),
        "{} is missing: the quest databases are versioned in data/ (git checkout -- data/)",
        packed.display()
    );
    Ok(path)
}

static EDITION: std::sync::OnceLock<Edition> = std::sync::OnceLock::new();

/// Game version given on the command line (`--edition`).
pub fn edition() -> Edition {
    EDITION.get().copied().unwrap_or_default()
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let edition = Edition::from_key(&cli.edition)
        .with_context(|| format!("unknown edition {} (forever, classic, tbc)", cli.edition))?;
    let _ = EDITION.set(edition);
    let overrides = PathBuf::from("overrides");
    let open = || -> Result<Connection> {
        let path = match &cli.db {
            Some(p) => p.clone(),
            None => database(edition)?,
        };
        Connection::open(&path).with_context(|| format!("opening {}", path.display()))
    };

    match cli.command {
        Command::Route {
            race,
            class,
            from_level,
            to_level,
            effort,
            construction,
            name,
            profession,
            allow_elite,
            no_dungeons,
            pvp_quests,
            progression,
            params,
            locale,
            players,
        } => route::run(
            &open()?,
            &route::RouteOptions {
                race,
                class,
                from_level,
                to_level: to_level.unwrap_or(edition.rules().max_level),
                effort,
                construction,
                out_name: name,
                allow_elite,
                no_dungeons,
                pvp_quests,
                progression,
                params,
                locale,
                start: players.start,
                mates: players.mates,
                profile_out: players.profile_out,
                professions: profession
                    .iter()
                    .map(|p| {
                        let (key, target) = p.split_once(':').unwrap_or((p, "300"));
                        fg_route::profession::ProfessionGoal {
                            key: key.to_owned(),
                            target: target.parse().unwrap_or(300),
                            start_level: None,
                            milestones: vec![],
                        }
                    })
                    .collect(),
                overrides,
                routes_dir: cli.routes.clone(),
                addon_dir: cli.addon_dir.clone(),
            },
        )?,
        Command::Show { name, full } => route::show(&cli.routes, &name, full)?,
        Command::Diff { a, b, json } => diff::run(&cli.routes, &a, b.as_deref(), json)?,
        Command::Addon => route::write_addon_routes(&open()?, &overrides, &cli.routes, &cli.addon_dir)?,
        Command::Polestar { out, source } => {
            let write = if source {
                fg_route::job::write_polestar_source
            } else {
                fg_route::job::write_polestar_pack
            };
            let n = write(&open()?, &cli.routes, &out, env!("CARGO_PKG_VERSION"))?;
            eprintln!("wrote {} ({n} guides)", out.display());
        }
        Command::Replay { file, effort } => route::replay(&open()?, &overrides, &file, effort, &cli.routes)?,
        Command::Travel { from, to, faction } => travel::run(&open()?, &overrides, &faction, &from, &to)?,
        Command::Audit { checks, all } => audit::run(&open()?, &overrides, &checks, all)?,
        Command::Talents { out } => talents::run(
            &open()?,
            &out.unwrap_or_else(|| PathBuf::from(format!("addon/Factoruide_Guide/TalentBuilds_{}.lua", edition.key()))),
            edition,
        )?,
    }
    Ok(())
}
