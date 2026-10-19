//! Data audits: each check looks for one kind of data problem everywhere in an edition's data,
//! for every race. A problem met once (a player's report) becomes a check, so that the same
//! problem is found in all the game's data, fixed there, and does not come back.
//!
//! Findings known to be right are accepted in `overrides/audit.toml` (with the reason); the
//! others are to fix.

use crate::job::{self, Overrides, PlanRequest};
use crate::model::{EntityKind, Model};
use crate::terrain::LARGE_AREA;
use crate::world::{Pos, World};
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

/// A check: its name, what it looks for, and whether its findings are errors (else a list to
/// review: candidates that are mostly right).
pub struct Check {
    pub name: &'static str,
    pub what: &'static str,
    pub error: bool,
}

/// The checks.
pub const CHECKS: &[Check] = &[
    Check {
        name: "islands",
        what: "places of quests in an area left only by boat, portal or lift ([[enclosed]] of \
               passes.toml) that none takes to: the planner cannot get there",
        error: true,
    },
    Check {
        name: "source-items",
        what: "items a quest needs (used on its objectives) that no step of the guide gets",
        error: true,
    },
    Check {
        name: "class-quests",
        what: "class quests every guide of the class must include (class_quests.toml) that the \
               planner leaves out for a race of that class: their place, a prerequisite or the \
               way there is unknown",
        error: true,
    },
    Check {
        name: "other-class-quests",
        what: "quests in a guide of a class given or turned in by another class's trainer: a \
               class quest whose class the data does not know, planned for every class",
        error: true,
    },
    Check {
        name: "terrain-gaps",
        what: "areas with places of quests the terrain does not connect to their continent's \
               mainland, even through the passages: the planner walks there in a straight line. \
               Mostly tunnels, bridges and gates, which are buildings (accept them); an island or \
               the top of a cliff left only by boat, portal or lift is to declare [[enclosed]] \
               in passes.toml, with its ways out in travel.toml",
        error: false,
    },
    Check {
        name: "chain-order",
        what: "WoW Forever quests new to it (not in the 1.12 data) with no prerequisite known, \
               given by an NPC who takes back another quest of the guide: the guide may take \
               them before the game offers them (a chain the sources lack). Check the quest's \
               series on Wowhead or in game: a missing prerequisite is fixed in the quest data, \
               a quest offered from the start is accepted",
        error: false,
    },
    Check {
        name: "broken-chains",
        what: "quests left out because the data does not know an objective or its place (an area \
               to reach, a mob, an object, an item's source) while quests of the guide need them \
               first: the rest of the chain is lost with them. Give the place in the quest data, \
               or accept a quest the game no longer offers",
        error: true,
    },
    Check {
        name: "child-quests",
        what: "quests handed in on the way to another's turn-in (its children: Distracting Jarven \
               for Bitter Rivals) that the guide cannot place where they end: the parent is \
               planned without them and its turn-in may be impossible. Give the place in the \
               quest data",
        error: true,
    },
];

/// Why a quest is left out when its objectives or their places are unknown to the data (the
/// reasons of the quest builder that say "unknown").
fn unknown_in_data(why: &str) -> bool {
    why.contains("unknown")
}

/// One problem found.
#[derive(Debug, Clone)]
pub struct Finding {
    pub check: &'static str,
    /// What the problem is about, stable from one data build to the next (to accept it).
    pub key: String,
    pub detail: String,
    /// The races whose guides have it.
    pub races: Vec<String>,
}

/// A finding known to be right, in `overrides/audit.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct Accepted {
    pub check: String,
    pub key: String,
    pub reason: String,
}

/// The accepted findings of an edition: those of `overrides/audit.toml` and of
/// `overrides/<edition>/audit.toml`.
pub fn accepted(dir: &std::path::Path, edition: crate::xp::Edition) -> Result<Vec<Accepted>> {
    #[derive(Default, Deserialize)]
    struct File {
        #[serde(default)]
        accept: Vec<Accepted>,
    }
    let mut accepted = Vec::new();
    let paths = [
        dir.join("audit.toml"),
        dir.join(edition.overrides_subdir()).join("audit.toml"),
    ];
    for path in paths {
        if let Ok(text) = std::fs::read_to_string(&path) {
            let file: File = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
            accepted.extend(file.accept);
        }
    }
    Ok(accepted)
}

/// Run the checks named (all when empty) for every race of the edition, a character of each
/// race leveling over the whole level range. Findings are merged over the races.
pub fn audit(
    conn: &Connection,
    overrides: &Overrides,
    checks: &[String],
    progress: &dyn Fn(&str),
) -> Result<Vec<Finding>> {
    let on = |name: &str| checks.is_empty() || checks.iter().any(|c| c == name);
    let mut found: BTreeMap<(&'static str, String), Finding> = BTreeMap::new();
    let source_items = if on("source-items") {
        quest_source_items(conn)?
    } else {
        HashMap::new()
    };
    let class_quests = job::class_quests(conn, overrides)?;
    // Forever only: it reworked zones with quests the sources often know without their chain;
    // TBC's new quests have Questie's.
    let chain_order_on = on("chain-order") && overrides.params.edition == crate::xp::Edition::Forever;
    let old_quests = if chain_order_on {
        old_quests(conn)?
    } else {
        HashSet::new()
    };
    let children = if on("child-quests") {
        child_links(conn)?
    } else {
        Vec::new()
    };
    let trainer_quests = if on("other-class-quests") {
        trainer_quests(conn)?
    } else {
        HashMap::new()
    };
    for (race, def) in &overrides.races {
        // Classic Era comparison databases have no class list: any class goes.
        let class = job::race_classes(conn, def.id)?
            .into_iter()
            .next()
            .unwrap_or_else(|| "warrior".into());
        let request = PlanRequest {
            race: race.clone(),
            class,
            from_level: 1,
            to_level: overrides.params.edition.rules().max_level,
            name: None,
            params: None,
            required_class_quests: None,
            professions: Vec::new(),
            locale: None,
            mates: Vec::new(),
            start: None,
        };
        let prepared = match job::prepare(conn, overrides, &request, &|_| {}) {
            Ok(p) => p,
            Err(e) => {
                progress(&format!("{race}: {e}"));
                continue;
            }
        };
        progress(&format!("{race}: {} quests", prepared.model.quests.len()));
        let mut add = |f: Finding| {
            let races = &mut found
                .entry((f.check, f.key.clone()))
                .or_insert_with(|| Finding { races: Vec::new(), ..f })
                .races;
            if !races.contains(race) {
                races.push(race.clone());
            }
        };
        if on("islands") {
            islands(&prepared.world, &prepared.model).into_iter().for_each(&mut add);
        }
        if on("terrain-gaps") {
            terrain_gaps(&prepared.world, &prepared.model)
                .into_iter()
                .for_each(&mut add);
        }
        if chain_order_on {
            chain_order(&prepared.model, &old_quests).into_iter().for_each(&mut add);
        }
        if on("broken-chains") {
            broken_chains(&prepared.model).into_iter().for_each(&mut add);
        }
        if on("child-quests") {
            children_left_out(&prepared.model, &children)
                .into_iter()
                .for_each(&mut add);
        }
        if on("source-items") {
            uncovered_source_items(&prepared.model, &source_items)
                .into_iter()
                .for_each(&mut add);
        }
        if on("class-quests") || on("other-class-quests") {
            for class in job::race_classes(conn, def.id)? {
                let required = on("class-quests") && class_quests.get(&class).is_some_and(|q| !q.is_empty());
                if !required && !on("other-class-quests") {
                    continue;
                }
                let request = PlanRequest {
                    class: class.clone(),
                    ..request.clone()
                };
                let p = match job::prepare(conn, overrides, &request, &|_| {}) {
                    Ok(p) => p,
                    Err(e) => {
                        progress(&format!("{race} {class}: {e}"));
                        continue;
                    }
                };
                if required {
                    let own = class_quest_ids(conn, p.profile.class_id)?;
                    class_quests_left_out(&class, &p.profile.required_class_quests, &own, &p.model)
                        .into_iter()
                        .for_each(&mut add);
                }
                if on("other-class-quests") {
                    other_class_quests(&class, &p.model, &trainer_quests)
                        .into_iter()
                        .for_each(&mut add);
                }
            }
        }
    }
    Ok(found.into_values().collect())
}

/// The quests of a class: filed under it or given to it only (a quest of another kind may have a
/// required class quest's name: Tirion Fordring's Redemption).
fn class_quest_ids(conn: &Connection, class_id: i64) -> Result<HashSet<i64>> {
    let mut stmt = conn.prepare("SELECT id FROM m_quest WHERE classes_mask = ?1 OR zone_or_sort = ?2")?;
    let ids = stmt
        .query_map(
            rusqlite::params![1i64 << (class_id - 1), -crate::model::class_sort(class_id)],
            |r| r.get(0),
        )?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

/// Required class quests of a class (by name) the model leaves out: skipped while building it,
/// or no longer required because a prerequisite cannot be planned.
fn class_quests_left_out(class: &str, required: &[String], own: &HashSet<i64>, model: &Model) -> Vec<Finding> {
    let skipped = (model.skipped.iter())
        .filter(|(id, name, why)| required.contains(name) && own.contains(id) && why != "repeatable")
        .map(|(id, name, why)| (*id, name, why.as_str()));
    // Of a name teaching a spell, only those leading to it are required.
    let designated = crate::model::required_by_name(&model.quests);
    let released = (model.quests.iter())
        .filter(|q| required.contains(&q.name) && own.contains(&q.id) && !q.mandatory)
        .filter(|q| designated.contains(&q.id))
        .map(|q| (q.id, &q.name, "a prerequisite cannot be planned"));
    skipped
        .chain(released)
        .map(|(id, name, why)| Finding {
            check: "class-quests",
            key: format!("{class} quest {id}"),
            detail: format!("{name} ({id}): {why}"),
            races: Vec::new(),
        })
        .collect()
}

/// Classes (keys) whose trainers give or take each quest, from the trainers' titles
/// ("Mage Trainer"; "Pet Trainer": the hunter's).
fn trainer_quests(conn: &Connection) -> Result<HashMap<i64, Vec<String>>> {
    let mut stmt = conn.prepare(
        "WITH t AS (SELECT r.quest_id, lower(substr(n.subname, 1, length(n.subname) - 8)) AS title
           FROM m_quest_relation r JOIN m_npc n ON n.id = r.entity_id
           WHERE r.entity_type = 'npc' AND n.subname LIKE '% Trainer')
         SELECT DISTINCT quest_id, CASE title WHEN 'pet' THEN 'hunter' ELSE title END AS class FROM t
         WHERE class IN (SELECT lower(Filename) FROM client_chrclasses)",
    )?;
    let mut out: HashMap<i64, Vec<String>> = HashMap::new();
    for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
        let (quest, class) = row?;
        out.entry(quest).or_default().push(class);
    }
    Ok(out)
}

/// Quests of the model of `class` that only other classes' trainers give or take.
fn other_class_quests(class: &str, model: &Model, trainer_quests: &HashMap<i64, Vec<String>>) -> Vec<Finding> {
    (model.quests.iter())
        .filter_map(|q| {
            let classes = trainer_quests.get(&q.id)?;
            (!classes.iter().any(|c| c == class)).then(|| Finding {
                check: "other-class-quests",
                key: format!("quest {}", q.id),
                detail: format!(
                    "{} ({}): {} trainer, in the {class}'s guide",
                    q.name,
                    q.id,
                    classes.join("/")
                ),
                races: Vec::new(),
            })
        })
        .collect()
}

/// A place of a quest: what is there, and its position.
struct Spot<'m> {
    quest: &'m str,
    what: String,
    pos: Pos,
}

/// A large walkable area of the terrain: continent and area.
type Area = (i64, u32);

/// Places of quests out of reach from `starts`, by area. `walk` tells whether walking goes from
/// an area to another, `passages` the areas joined by each passage; with `links`, boats, portals
/// and lifts (`travel.toml`) go from one to another. One finding per area, named after its zone
/// and the place most of its quests are at.
fn out_of_reach(
    check: &'static str,
    world: &World,
    model: &Model,
    starts: &[Area],
    walk: impl Fn(Area, Area) -> bool,
    passages: &[Vec<Area>],
    links: bool,
) -> Vec<Finding> {
    let area = |p: &Pos| large_area(world, p);
    let mut next: HashMap<Area, Vec<Area>> = HashMap::new();
    for l in world.links.iter().filter(|_| links) {
        if let (Some(a), Some(b)) = (area(&l.from), area(&l.to)) {
            next.entry(a).or_default().push(b);
        }
    }
    for areas in passages {
        for &a in areas {
            next.entry(a).or_default().extend(areas.iter().copied());
        }
    }
    let mut spots: BTreeMap<Area, Vec<Spot<'_>>> = BTreeMap::new();
    for q in &model.quests {
        let places = q
            .starts
            .iter()
            .map(|l| (format!("starts at {}", l.name), l))
            .chain(q.ends.iter().map(|l| (format!("ends at {}", l.name), l)))
            .chain(
                q.objectives
                    .iter()
                    .filter(|o| o.dungeon.is_none())
                    .map(|o| (o.text.clone(), &o.loc)),
            );
        for (what, loc) in places {
            if loc.kind == EntityKind::Dungeon {
                continue;
            }
            if let Some(a) = area(&loc.pos) {
                spots.entry(a).or_default().push(Spot {
                    quest: &q.name,
                    what,
                    pos: loc.pos,
                });
            }
        }
    }
    let mut reached: HashSet<Area> = starts.iter().copied().collect();
    let mut queue: VecDeque<Area> = starts.iter().copied().collect();
    while let Some(a) = queue.pop_front() {
        let walked = spots.keys().copied().filter(|&b| walk(a, b));
        for b in next
            .get(&a)
            .into_iter()
            .flatten()
            .copied()
            .chain(walked)
            .collect::<Vec<_>>()
        {
            if reached.insert(b) {
                queue.push_back(b);
            }
        }
    }
    spots
        .into_iter()
        .filter(|(a, _)| !reached.contains(a))
        .map(|(_, spots)| {
            let mut places: BTreeMap<String, usize> = BTreeMap::new();
            for s in &spots {
                let zone = world.zone_at(&s.pos).map_or("?", |z| world.zone_name(z));
                let place = world.place_name(&s.pos).unwrap_or("");
                *places.entry(format!("{zone}: {place}")).or_default() += 1;
            }
            let key = places
                .iter()
                .max_by_key(|(name, n)| (**n, std::cmp::Reverse((*name).clone())))
                .map(|(name, _)| name.clone())
                .unwrap_or_default();
            let quests: std::collections::BTreeSet<&str> = spots.iter().map(|s| s.quest).collect();
            let s = &spots[0];
            let at = world
                .zone_at(&s.pos)
                .and_then(|z| world.to_map(z, &s.pos))
                .map(|m| format!(" ({} {:.1}, {:.1})", world.zone_name(m.zone), m.x, m.y))
                .unwrap_or_default();
            Finding {
                check,
                key,
                detail: format!(
                    "{} places of {} quests, e.g. {}: {}{at}",
                    spots.len(),
                    quests.len(),
                    s.quest,
                    s.what
                ),
                races: Vec::new(),
            }
        })
        .collect()
}

fn large_area(world: &World, p: &Pos) -> Option<Area> {
    world
        .terrain
        .area_of(p)
        .filter(|&(a, size)| a != 0 && size >= LARGE_AREA)
        .map(|(a, _)| (p.continent, a))
}

/// Places of quests the planner cannot get to: in an area left only by boat, portal or lift
/// (`[[enclosed]]`) that none takes to, or from one.
fn islands(world: &World, model: &Model) -> Vec<Finding> {
    let enclosed = |a: Area| world.terrain.is_enclosed(a.0, a.1);
    let walk = |a: Area, b: Area| a.0 == b.0 && !enclosed(a) && !enclosed(b);
    let Some(start) = large_area(world, &model.start.pos) else {
        return Vec::new();
    };
    out_of_reach("islands", world, model, &[start], walk, &[], true)
}

/// Yards around a passage where the areas it joins are looked for (the two ends of a tunnel).
const PASSAGE_REACH: f64 = 150.0;

/// Places of quests in an area the terrain does not connect to its continent's mainland (its
/// largest area), even through the passages (the areas around each), and not declared
/// enclosed: the planner walks there in a straight line. Either a tunnel, a bridge or a gate the
/// terrain does not hold (accept it), or an area left only by boat, portal or lift to declare
/// `[[enclosed]]`, with its ways out in `travel.toml`. A boat does not count: it lands on
/// islands.
fn terrain_gaps(world: &World, model: &Model) -> Vec<Finding> {
    let passages: Vec<Vec<Area>> = world
        .passes
        .iter()
        .map(|pass| {
            let mut areas: Vec<Area> = Vec::new();
            for r in [0.0, 50.0, 100.0, PASSAGE_REACH] {
                for k in 0..12 {
                    let angle = f64::from(k) * std::f64::consts::PI / 6.0;
                    let p = Pos {
                        x: pass.pos.x + r * angle.cos(),
                        y: pass.pos.y + r * angle.sin(),
                        ..pass.pos
                    };
                    if let Some(a) = large_area(world, &p).filter(|a| !areas.contains(a)) {
                        areas.push(a);
                    }
                }
            }
            areas
        })
        .collect();
    let mut mainlands: Vec<Area> = world
        .terrain
        .continents()
        .into_iter()
        .filter_map(|c| world.terrain.main_area(c).map(|a| (c, a)))
        .collect();
    mainlands.extend(world.terrain.enclosed_areas());
    out_of_reach(
        "terrain-gaps",
        world,
        model,
        &mainlands,
        |a, b| a == b,
        &passages,
        false,
    )
}

/// What each quest needs on top of its objectives: the items used on them (required source
/// items) and whether each one is found somewhere (dropped, looted, sold; not in what the
/// quest gives on accepting, a bag of them), the one given on
/// accepting, the items its objectives ask for, and whether one of those is made of the others
/// (found nowhere itself, nor is what holds it: Morrowgrain, grown from soil and seeds).
struct SourceItems {
    items: Vec<(i64, String, bool)>,
    given: Option<i64>,
    asked: Vec<i64>,
    assembled: bool,
}

fn quest_source_items(conn: &Connection) -> Result<HashMap<i64, SourceItems>> {
    let mut stmt = conn.prepare(
        "SELECT q.id, j.value, q.source_item_id, coalesce(i.name, ''),
                EXISTS (SELECT 1 FROM m_item_source s WHERE s.item_id = j.value)
                  AND NOT EXISTS (SELECT 1 FROM m_item_source s WHERE s.item_id = j.value
                    AND s.from_type = 'item' AND s.from_id IS q.source_item_id),
                (SELECT group_concat(o.target_id) FROM m_quest_objective o
                  WHERE o.quest_id = q.id AND o.type = 'item'),
                EXISTS (SELECT 1 FROM m_quest_objective o
                  WHERE o.quest_id = q.id AND o.type = 'item'
                    AND NOT EXISTS (SELECT 1 FROM m_item_source s
                      LEFT JOIN m_item_source c ON s.from_type = 'item' AND c.item_id = s.from_id
                      WHERE s.item_id = o.target_id
                        AND (s.from_type != 'item' OR c.from_type != 'item')))
         FROM m_quest q, json_each(json_extract(q.extra, '$.required_source_items')) j
         LEFT JOIN m_item i ON i.id = j.value",
    )?;
    let mut out: HashMap<i64, SourceItems> = HashMap::new();
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Option<i64>>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, bool>(4)?,
            r.get::<_, Option<String>>(5)?,
            r.get::<_, bool>(6)?,
        ))
    })?;
    for row in rows {
        let (quest, item, given, name, found, asked, assembled) = row?;
        let e = out.entry(quest).or_insert_with(|| SourceItems {
            items: Vec::new(),
            given,
            asked: asked
                .unwrap_or_default()
                .split(',')
                .filter_map(|s| s.parse().ok())
                .collect(),
            assembled,
        });
        e.items.push((item, name, found));
    }
    Ok(out)
}

/// Quests of the guide needing an item found somewhere that none of its objectives gets (nor
/// is it given on accepting, nor are they parts of an item asked for, nor got in a dungeon run).
fn uncovered_source_items(model: &Model, source_items: &HashMap<i64, SourceItems>) -> Vec<Finding> {
    let mut out = Vec::new();
    for q in &model.quests {
        let Some(needs) = source_items.get(&q.id) else {
            continue;
        };
        if needs.assembled || q.objectives.iter().any(|o| o.dungeon.is_some()) {
            continue;
        }
        for (item, name, found) in &needs.items {
            let got = |o: &crate::model::Objective| o.loc.kind == EntityKind::Item && o.loc.id == *item;
            if !found
                || needs.given == Some(*item)
                || needs.asked.contains(item)
                || q.objectives.iter().any(|o| o.places().any(got))
            {
                continue;
            }
            out.push(Finding {
                check: "source-items",
                key: format!("quest {} item {item}", q.id),
                detail: format!("{} ({}) needs {name} ({item}): no step gets it", q.name, q.id),
                races: Vec::new(),
            });
        }
    }
    out
}

/// The quests of the 1.12 data (VMaNGOS): the server's own prerequisites, complete.
fn old_quests(conn: &Connection) -> Result<HashSet<i64>> {
    let mut stmt = conn.prepare("SELECT id FROM m_quest WHERE ',' || sources || ',' LIKE '%,vmangos,%'")?;
    let ids = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

/// Quests of the guide not in the 1.12 data, with no prerequisite, given by an NPC who takes back
/// another quest of the guide not linked to them: Walton gave Trouble in the Valley only after The
/// Hermit Tanner, which no source said (the guide took it first).
fn chain_order(model: &Model, old_quests: &HashSet<i64>) -> Vec<Finding> {
    let before = |id: i64| {
        let mut seen = HashSet::new();
        let mut todo = vec![id];
        while let Some(q) = todo.pop() {
            let Some(&i) = model.index.get(&q) else { continue };
            for &p in model.quests[i].pre_all.iter().chain(&model.quests[i].pre_any) {
                if seen.insert(p) {
                    todo.push(p);
                }
            }
        }
        seen
    };
    let npc = |l: &crate::model::Loc| (l.kind == EntityKind::Npc).then_some(l.id);
    let mut out = Vec::new();
    for b in &model.quests {
        if old_quests.contains(&b.id) || !b.pre_all.is_empty() || !b.pre_any.is_empty() {
            continue;
        }
        for start in b.starts.iter().filter_map(|l| npc(l).map(|id| (id, &l.name))) {
            let taken_back: Vec<String> = (model.quests.iter())
                .filter(|a| {
                    let linked = a.id == b.id
                        || a.exclusive.contains(&b.id)
                        || b.exclusive.contains(&a.id)
                        || a.breadcrumb_for == Some(b.id)
                        || b.breadcrumb_for == Some(a.id)
                        || before(a.id).contains(&b.id);
                    !linked && a.min_level <= b.min_level && a.ends.iter().any(|l| npc(l) == Some(start.0))
                })
                .map(|a| format!("{} ({})", a.name, a.id))
                .collect();
            if taken_back.is_empty() {
                continue;
            }
            out.push(Finding {
                check: "chain-order",
                key: format!("quest {}", b.id),
                detail: format!(
                    "{} ({}) from {}, who takes back {}: no prerequisite known",
                    b.name,
                    b.id,
                    start.1,
                    taken_back.join(", ")
                ),
                races: Vec::new(),
            });
        }
    }
    out
}

/// (parent, parent's name, child, child's name) of the children the guides hand in on the way
/// (the repeatable ones, see the model's `LinkKind::Child`).
fn child_links(conn: &Connection) -> Result<Vec<(i64, String, i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.name, c.id, c.name FROM m_quest_link l
         JOIN m_quest p ON p.id = l.quest_id JOIN m_quest c ON c.id = l.other_id
         WHERE l.kind = 'child' AND coalesce(c.special_flags, 0) & 1 != 0 ORDER BY p.id, c.id",
    )?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Planned quests one of whose children has no objective handing it in.
fn children_left_out(model: &Model, children: &[(i64, String, i64, String)]) -> Vec<Finding> {
    let mut out = Vec::new();
    for (parent, parent_name, child, child_name) in children {
        let Some(&i) = model.index.get(parent) else { continue };
        if model.quests[i].objectives.iter().any(|o| o.child == Some(*child)) {
            continue;
        }
        out.push(Finding {
            check: "child-quests",
            key: format!("quest {parent} child {child}"),
            detail: format!(
                "{parent_name} ({parent}) is planned without {child_name} ({child}): where it is handed in is unknown"
            ),
            races: Vec::new(),
        });
    }
    out
}

/// Quests left out for something the data does not know (`unknown_in_data`) that quests of the
/// guide need first: all of them (`pre_all`), or the only ones of a choice (`pre_any`, all left
/// out). Secure the Mountain's den had no place, which lost the report taken back after it.
fn broken_chains(model: &Model) -> Vec<Finding> {
    let unknown: HashMap<i64, (&str, &str)> = (model.skipped.iter())
        .filter(|(_, _, why)| unknown_in_data(why))
        .map(|(id, name, why)| (*id, (name.as_str(), why.as_str())))
        .collect();
    let mut needed: BTreeMap<i64, Vec<String>> = BTreeMap::new();
    for q in &model.quests {
        let any_lost = !q.pre_any.is_empty() && q.pre_any.iter().all(|p| !model.index.contains_key(p));
        let lost = (q.pre_all.iter())
            .chain(q.pre_any.iter().filter(|_| any_lost))
            .filter(|p| unknown.contains_key(p));
        for p in lost {
            let by = needed.entry(*p).or_default();
            let next = format!("{} ({})", q.name, q.id);
            if !by.contains(&next) {
                by.push(next);
            }
        }
    }
    needed
        .into_iter()
        .map(|(id, by)| {
            let (name, why) = unknown[&id];
            Finding {
                check: "broken-chains",
                key: format!("quest {id}"),
                detail: format!("{name} ({id}) left out: {why}; needed first by {}", by.join(", ")),
                races: Vec::new(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::tests::quests_at_start;

    /// The accepted findings of the repository's three editions read (a bad line stops `fg audit`).
    #[test]
    fn the_accepted_findings_files_read() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../overrides");
        for edition in crate::xp::Edition::ALL {
            let found = accepted(&dir, edition).unwrap();
            assert!(!found.is_empty(), "{edition:?}");
        }
    }

    /// Secure the Mountain's case: a quest left out because an objective has no place, needed
    /// first by a quest of the guide. Not found for a reason the data does not cause (elites),
    /// nor when another quest of a choice stays.
    #[test]
    fn a_quest_left_out_for_an_unknown_place_that_a_kept_quest_needs_is_found() {
        let mut p = quests_at_start(2);
        let first = p.model.quests[0].id;
        let lost = 990_001;
        p.model.skipped = vec![
            (lost, "Secure the Mountain".into(), "exploration area unknown".into()),
            (990_002, "Elites".into(), "elite objective".into()),
        ];
        let keys = |m: &Model| -> Vec<String> { broken_chains(m).into_iter().map(|f| f.key).collect() };
        assert!(keys(&p.model).is_empty(), "nothing needs it");
        p.model.quests[0].pre_all = vec![lost];
        assert_eq!(keys(&p.model), [format!("quest {lost}")]);
        p.model.quests[0].pre_all = vec![990_002];
        assert!(keys(&p.model).is_empty(), "left out for an elite, not the data");
        p.model.quests[0].pre_all.clear();
        p.model.quests[1].pre_any = vec![lost, first];
        assert!(keys(&p.model).is_empty(), "the other quest of the choice stays");
        p.model.quests[1].pre_any = vec![lost];
        assert_eq!(keys(&p.model), [format!("quest {lost}")]);
    }

    /// Walton's case: two quests of one NPC, one he also takes back, no link between them. Found
    /// until the data links them, or when the quest is in the 1.12 data.
    #[test]
    fn a_quest_given_by_the_npc_taking_back_another_is_found_until_linked() {
        let mut p = quests_at_start(2);
        let (hermit, trouble) = (p.model.quests[0].id, p.model.quests[1].id);
        let found = |model: &Model, old: &HashSet<i64>| -> Vec<String> {
            chain_order(model, old).into_iter().map(|f| f.key).collect()
        };
        // Each is given by the NPC who takes back the other.
        assert_eq!(
            found(&p.model, &HashSet::new()),
            [format!("quest {hermit}"), format!("quest {trouble}")]
        );
        assert_eq!(found(&p.model, &HashSet::from([hermit])), [format!("quest {trouble}")]);
        p.model.quests[1].pre_all = vec![hermit];
        assert!(found(&p.model, &HashSet::new()).is_empty(), "linked both ways");
        // A quest asking a lower level cannot need one asking a higher one.
        p.model.quests[1].pre_all.clear();
        p.model.quests[0].min_level = 10;
        assert_eq!(found(&p.model, &HashSet::new()), [format!("quest {hermit}")]);
    }
}
