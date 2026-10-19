//! `fg route`: plan a leveling route for a race/class and export it for the addon.

use anyhow::{Context, Result};
use fg_route::Route;
use fg_route::export::StepKind;
use fg_route::job::{self, Overrides, PlanRequest};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

pub struct RouteOptions {
    pub race: String,
    pub class: String,
    pub from_level: i64,
    pub to_level: i64,
    pub effort: Option<f64>,
    pub construction: Option<String>,
    pub out_name: Option<String>,
    pub allow_elite: bool,
    pub no_dungeons: bool,
    pub pvp_quests: bool,
    /// Progression profile (cautious, normal, risky).
    pub progression: Option<String>,
    /// Planner parameter overrides (key=value).
    pub params: Vec<String>,
    pub professions: Vec<fg_route::profession::ProfessionGoal>,
    /// Language of the guide texts.
    pub locale: Option<String>,
    /// Profiles (JSON) of the character and of the other players of a group guide, and where to
    /// write the character's at the end.
    pub start: Option<PathBuf>,
    pub mates: Vec<PathBuf>,
    pub profile_out: Option<PathBuf>,
    pub overrides: PathBuf,
    pub routes_dir: PathBuf,
    pub addon_dir: PathBuf,
}

pub fn run(conn: &Connection, opts: &RouteOptions) -> Result<()> {
    let overrides = Overrides::load_edition(&opts.overrides, crate::edition())?;
    let mut params = overrides.params.with_overrides(&opts.params)?;
    if let Some(e) = opts.effort {
        params.effort = e;
    }
    if let Some(c) = &opts.construction {
        params = params.with_overrides(&[format!("construction={c}")])?;
    }
    if opts.allow_elite {
        params.allow_elite = true;
    }
    if opts.no_dungeons {
        params.dungeons = false;
    }
    if opts.pvp_quests {
        params.pvp_quests = true;
    }
    if let Some(p) = &opts.progression {
        params = params.with_overrides(&[format!("progression={p}")])?;
    }
    let request = PlanRequest {
        race: opts.race.clone(),
        class: opts.class.clone(),
        from_level: opts.from_level,
        to_level: opts.to_level,
        name: opts.out_name.clone(),
        params: Some(params),
        required_class_quests: None,
        professions: opts.professions.clone(),
        locale: opts.locale.clone(),
        start: opts.start.as_deref().map(read_profile).transpose()?,
        mates: opts.mates.iter().map(|p| read_profile(p)).collect::<Result<_>>()?,
    };
    let outcome = job::plan(conn, &overrides, &request, &|m| {
        if !m.starts_with('@') {
            eprintln!("{m}");
        }
    })?;
    for note in &outcome.notes {
        eprintln!("  {note}");
    }
    let route = &outcome.route;
    let b = &route.breakdown;
    let m = fg_route::plan::fmt_time;
    eprintln!(
        "  travel {} | flights {} | fighting {} | grinding {} (over cap {}) | farming on the way {} | overhead {}",
        m(b.travel),
        m(b.flights),
        m(b.fighting),
        m(b.grinding),
        m(b.grind_over),
        m(b.farming),
        m(b.overhead)
    );
    eprintln!(
        "  xp: quests {} | quest mobs {} | grinding {} | farming on the way {} | exploration {}",
        b.quest_xp, b.mob_xp, b.grind_xp, b.farm_xp, b.explore_xp
    );
    eprintln!(
        "  training {} | hearthstones {} | class quests missing {} | zone quests missing {}",
        m(b.training),
        b.hearths,
        b.missing_class_quests,
        b.missing_zone_quests
    );
    eprintln!("  dungeons: {} runs, {}", b.dungeon_runs, m(b.dungeons));
    for r in &route.reputation {
        eprintln!(
            "  reputation {}: {} -> {} (goal {})",
            r.name, r.start, r.reached, r.target
        );
    }
    if b.sync_time > 0.0 || b.desync > 0.0 {
        eprintln!(
            "  group levels together after {} (levels apart beyond the window: {:.0} level-seconds)",
            m(b.sync_time),
            b.desync
        );
    }
    if let (Some(path), Some(end)) = (&opts.profile_out, &outcome.end) {
        std::fs::write(path, serde_json::to_string_pretty(end)?)?;
        eprintln!("wrote {}", path.display());
    }
    job::save_route(&opts.routes_dir, route)?;
    write_addon_routes(conn, &opts.overrides, &opts.routes_dir, &opts.addon_dir)
}

/// A character's profile (JSON, as `--profile-out` writes it).
fn read_profile(path: &Path) -> Result<job::StartState> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("{} is not a profile", path.display()))
}

/// Regenerate the addon's Routes.lua from every route JSON (each in its own language), and
/// Services.lua.
pub fn write_addon_routes(conn: &Connection, overrides_dir: &Path, routes_dir: &Path, addon_dir: &Path) -> Result<()> {
    let overrides = Overrides::load_edition(overrides_dir, crate::edition())?;
    let n = job::write_addon_routes(conn, &overrides, crate::edition(), routes_dir, addon_dir)?;
    eprintln!("wrote {} ({n} routes)", addon_dir.join("Routes.lua").display());
    Ok(())
}

/// Print a route as zone segments, with time and level at each change.
pub fn show(routes_dir: &Path, name: &str, full: bool) -> Result<()> {
    let path = routes_dir.join(format!("{}.json", job::slug(name)));
    let route: Route = fg_route::export::read_route(serde_json::from_str(
        &std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?,
    )?)?;
    let t = fg_route::plan::fmt_time;
    println!(
        "{}: level {} -> {} in {}, {} quests, {} steps",
        route.name,
        route.from_level,
        route.to_level,
        route.total_time_text,
        route.quests,
        route.steps.len()
    );
    let b = &route.breakdown;
    println!(
        "travel {} | flights {} | fighting {} | grinding {} | farming on the way {} | overhead {}",
        t(b.travel),
        t(b.flights),
        t(b.fighting),
        t(b.grinding),
        t(b.farming),
        t(b.overhead)
    );
    let mut zone = String::new();
    for (i, s) in route.steps.iter().enumerate() {
        let z = s.zone.clone().unwrap_or_default();
        if full || z != zone || matches!(s.kind, StepKind::Fly | StepKind::Grind) {
            let along = if s.bg { "  (along the way)" } else { "" };
            println!(
                "{:>4} {:>6} L{:<2} {:<20} {}{along}",
                i + 1,
                t(s.time),
                s.level,
                z,
                s.text
            );
        }
        zone = z;
    }
    Ok(())
}

/// Replay a request exported by the app (debug/request-*.json).
pub fn replay(
    conn: &Connection,
    overrides_dir: &Path,
    file: &Path,
    effort: Option<f64>,
    routes_dir: &Path,
) -> Result<()> {
    let mut request: PlanRequest =
        serde_json::from_str(&std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?)?;
    let overrides = Overrides::load_edition(overrides_dir, crate::edition())?;
    if let Some(e) = effort {
        request.params.get_or_insert_with(|| overrides.params.clone()).effort = e;
    }
    request.name = Some(format!(
        "{}-replay",
        request.name.clone().unwrap_or_else(|| "request".into())
    ));
    let outcome = job::plan(conn, &overrides, &request, &|m| {
        if !m.starts_with('@') {
            eprintln!("{m}");
        }
    })?;
    for note in &outcome.notes {
        eprintln!("  {note}");
    }
    job::save_route(routes_dir, &outcome.route)?;
    eprintln!("saved {} (fg show {} --full)", outcome.route.name, outcome.route.name);
    Ok(())
}
