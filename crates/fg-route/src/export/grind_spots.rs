//! Where the guide has the character grind: planned grinds and checkpoints are moved back to the
//! last place of the route where mobs of its level live (a city has none), as an XP target
//! there, rather than a trip to grind and back.

use super::types::{Checkpoint, Step, StepKind};
use super::writer::StepWriter;
use crate::world::Pos;

/// A step is a place to grind when this many mobs of the character's level live within
/// `RADIUS` yards of it.
const RADIUS: f64 = 250.0;
const MIN_MOBS: usize = 8;
/// Between checkpoints, every level has its own step too, letting the character be this share
/// of a level behind the route before it asks to grind.
const LEVEL_SLACK: f64 = 0.1;

/// Steps of the guide with the XP the route has after each one (in its level).
pub(super) struct Guide<'s> {
    pub(super) steps: &'s mut Vec<Step>,
    pub(super) xps: &'s mut Vec<i64>,
    pub(super) checkpoints: &'s mut Vec<Checkpoint>,
}

impl Guide<'_> {
    fn insert(&mut self, at: usize, step: Step, xp: i64) {
        self.steps.insert(at, step);
        self.xps.insert(at, xp);
        for c in self.checkpoints.iter_mut().filter(|c| c.step >= at) {
            c.step += 1;
        }
    }

    fn remove(&mut self, at: usize) {
        self.steps.remove(at);
        self.xps.remove(at);
        for c in self.checkpoints.iter_mut().filter(|c| c.step > at) {
            c.step -= 1;
        }
    }

    /// First step after the last grind before `at`: grinds are not moved past another one.
    fn floor(&self, at: usize) -> usize {
        self.steps[..at]
            .iter()
            .rposition(|s| s.kind == StepKind::Grind)
            .map_or(0, |k| k + 1)
    }
}

/// Planned grinds and checkpoints placed where mobs of the character's level live, and a step at
/// every other level.
pub(super) fn place(writer: &StepWriter<'_>, guide: &mut Guide<'_>) {
    move_grinds(writer, guide);
    let checkpoints: Vec<i64> = guide.checkpoints.iter().map(|c| c.level).collect();
    for n in 0..guide.checkpoints.len() {
        let (at, level) = (guide.checkpoints[n].step, guide.checkpoints[n].level);
        // Be where the route is at the last place to grind before the level.
        if let Some(k) = spot(writer, guide.steps, guide.floor(at), at, level - 1) {
            let xp = guide.xps[k];
            let step = writer.checkpoint(&guide.steps[k], level, level - 1, xp);
            guide.insert(k + 1, step, xp);
        } else {
            // Before the run when the level is reached inside a dungeon.
            let mut at = at;
            while at > 1 && guide.steps[at].inside {
                at -= 1;
            }
            let step = writer.checkpoint(&guide.steps[at - 1], level, level, 0);
            guide.insert(at, step, 0);
            guide.checkpoints[n].step = at;
        }
    }
    levels(writer, guide, &checkpoints);
}

/// A step before each level the route reaches (but the checkpoints'): a character more than
/// `LEVEL_SLACK` of a level behind grinds at the last place of the level where its mobs live,
/// instead of going on into content made for a higher level. None where no such place is (the
/// next level's step catches up).
fn levels(writer: &StepWriter<'_>, guide: &mut Guide<'_>, checkpoints: &[i64]) {
    let rules = writer.planner.rules();
    let profile = writer.planner.profile;
    for level in profile.from_level + 2..profile.to_level {
        if checkpoints.contains(&level) {
            continue;
        }
        let Some(at) = guide.steps.iter().position(|s| s.level >= level) else {
            break;
        };
        let Some(k) = spot(writer, guide.steps, guide.floor(at), at, level - 1) else {
            continue;
        };
        let xp = guide.xps[k] - (LEVEL_SLACK * rules.to_next_level(level - 1) as f64) as i64;
        if xp <= 0 {
            continue;
        }
        let step = writer.catch_up(&guide.steps[k], level, level - 1, xp);
        guide.insert(k + 1, step, guide.xps[k]);
    }
}

/// Planned grinds where no mob of the character's level lives (a city, a flight path) are
/// done earlier at the last place of the route where some do, to the XP that leaves the route
/// on schedule.
fn move_grinds(writer: &StepWriter<'_>, guide: &mut Guide<'_>) {
    let rules = writer.planner.rules();
    let total = |level: i64, xp: i64| (1..level).map(|l| rules.to_next_level(l)).sum::<i64>() + xp;
    let split = |mut xp: i64| {
        let mut level = 1;
        while level < rules.max_level && xp >= rules.to_next_level(level) {
            xp -= rules.to_next_level(level);
            level += 1;
        }
        (level, xp)
    };
    let mut i = 1;
    while i < guide.steps.len() {
        let (grind, before) = (&guide.steps[i], &guide.steps[i - 1]);
        let level = before.level;
        // A group's players catching up stay where they are (the others wait there).
        if grind.kind != StepKind::Grind
            || grind.xp.is_some()
            || grind.members.is_some()
            || grindable(writer, grind, level)
        {
            i += 1;
            continue;
        }
        let Some(k) = spot(writer, guide.steps, guide.floor(i), i, level) else {
            i += 1;
            continue;
        };
        let gained = total(grind.level, guide.xps[i]) - total(level, guide.xps[i - 1]);
        let (to, xp) = split(total(guide.steps[k].level, guide.xps[k]) + gained);
        // Step times are when each ends: the grind ends its length after step `k`, the steps it
        // now comes before end that much later.
        let length = grind.time - before.time;
        let minutes = (length / 60.0).ceil();
        let mut step = writer.grind_before(grind, &guide.steps[k], to, xp, minutes);
        step.time = guide.steps[k].time + length;
        guide.remove(i);
        for later in &mut guide.steps[k + 1..i] {
            later.time += length;
        }
        guide.insert(k + 1, step, xp);
        // The steps it now comes before have that XP too (above their level when it crosses one).
        for x in &mut guide.xps[k + 2..=i] {
            *x += gained;
        }
        i += 1;
    }
}

/// Last step of `floor..at` still at `level` where mobs of that level live (not a flight, a
/// boat, the hearthstone or a teleport: the character is no longer where they start; nor a
/// dungeon run or a quest done in it before another: the run's steps stay together).
fn spot(writer: &StepWriter<'_>, steps: &[Step], floor: usize, at: usize, level: i64) -> Option<usize> {
    (floor..at).rev().take_while(|&k| steps[k].level == level).find(|&k| {
        !matches!(
            steps[k].kind,
            StepKind::Fly | StepKind::Link | StepKind::Hearth | StepKind::Teleport
        ) && !steps.get(k + 1).is_some_and(|s| s.inside)
            && grindable(writer, &steps[k], level)
    })
}

/// Whether mobs a character of `level` grinds live around a step.
fn grindable(writer: &StepWriter<'_>, step: &Step, level: i64) -> bool {
    let params = writer.planner.params;
    step.world.is_some_and(|[continent, x, y]| {
        let p = Pos {
            continent: continent as i64,
            x,
            y,
            zone: 0,
        };
        writer
            .planner
            .model
            .farm
            .around(&p, RADIUS, level - params.farm_below..=level + params.farm_above)
            >= MIN_MOBS
    })
}
