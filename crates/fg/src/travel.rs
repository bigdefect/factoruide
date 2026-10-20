//! `fg travel`: how we walk between two points (distance, passages), to check travel costs.

use anyhow::{Context, Result};
use fg_route::job::Overrides;
use fg_route::world::World;
use rusqlite::Connection;
use std::path::Path;

fn point(world: &World, spec: &str) -> Result<fg_route::world::Pos> {
    let parts: Vec<&str> = spec.split(',').map(str::trim).collect();
    anyhow::ensure!(parts.len() == 3, "expected Zone,x,y: {spec}");
    let zone = match parts[0].parse::<i64>() {
        Ok(id) => id,
        Err(_) => *world
            .zone_names
            .iter()
            .filter(|(_, n)| n.eq_ignore_ascii_case(parts[0]))
            .map(|(id, _)| id)
            .min()
            .with_context(|| format!("unknown zone {}", parts[0]))?,
    };
    world
        .to_world(zone, parts[1].parse()?, parts[2].parse()?)
        .with_context(|| format!("no map for zone {zone}"))
}

pub fn run(conn: &Connection, overrides_dir: &Path, faction: &str, from: &str, to: &str) -> Result<()> {
    let overrides = Overrides::load_edition(overrides_dir, crate::edition())?;
    let params = &overrides.params;
    let faction =
        fg_route::faction::Faction::parse(faction).ok_or_else(|| anyhow::anyhow!("unknown faction {faction}"))?;
    let mut world = World::load(conn, faction, params.flight_speed)?;
    for w in world.enclose(&overrides.enclosed) {
        eprintln!("{w}");
    }
    world.raise(&overrides.raised);
    for w in world.add_passes(conn, &overrides.passes)? {
        eprintln!("{w}");
    }
    let (a, b) = (point(&world, from)?, point(&world, to)?);
    let (d, passes) = world.walk_route(&a, &b);
    println!(
        "straight {:.0} yd, walking {:.0} yd = {:.1} min on foot",
        a.dist(&b),
        d,
        d * params.detour / params.run_speed / 60.0
    );
    let planned = world.walk_distance(&a, &b);
    if (planned - d).abs() > 1e-6 && !(planned.is_infinite() && d.is_infinite()) {
        println!("the planner walks {planned:.0} yd (straight line where the walk is unknown)");
    }
    if let (Some(ta), Some(tb)) = (world.terrain.area_of(&a), world.terrain.area_of(&b)) {
        let how = match world.terrain.distance(&a, &b, 0) {
            None => "way unknown (straight line assumed)",
            Some(w) if w.is_infinite() => "an enclosed area and another (no walk)",
            Some(w) if (w - a.dist(&b)).abs() < 1e-6 => "straight line clear",
            Some(_) => "path around the steep ground",
        };
        println!(
            "terrain: area {} ({} cells) to area {} ({} cells), {how}",
            ta.0, ta.1, tb.0, tb.1
        );
    }
    if let Some((i, j)) = passes {
        let p = |k: usize| {
            format!(
                "{} ({:.0},{:.0})",
                world.passes[k].name, world.passes[k].pos.x, world.passes[k].pos.y
            )
        };
        println!(
            "leaves {} by {}, enters {} by {}",
            world.zone_name(a.zone),
            p(i),
            world.zone_name(b.zone),
            p(j)
        );
    }
    Ok(())
}
