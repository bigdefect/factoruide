//! A guide replayed to be watched, on the Classic database: its times are the guide's, the
//! character moves to where each step is done, and what it holds follows the steps.

use fg_route::export::StepKind;
use fg_route::job::{self, Editor, LegMode, Overrides, PlanOutcome, PlanRequest, Timeline};
use fg_route::model::Names;
use fg_route::xp::Edition;
use rusqlite::Connection;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The Classic database, unpacked next to the packed one when missing or older.
fn database() -> Connection {
    static UNPACK: Mutex<()> = Mutex::new(());
    let _guard = UNPACK.lock().unwrap();
    let (path, packed) = (
        repo().join("data/classic.sqlite"),
        repo().join("data/classic.sqlite.zst"),
    );
    let modified = |p: &PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    if !path.exists() || modified(&packed) > modified(&path) {
        let data = zstd::decode_all(std::fs::File::open(&packed).unwrap()).unwrap();
        std::fs::write(&path, data).unwrap();
    }
    Connection::open(path).unwrap()
}

fn overrides() -> Overrides {
    Overrides::load_edition(&repo().join("overrides"), Edition::Classic).unwrap()
}

/// An orc warlock to 25 (its first routes, no optimization): it flies and takes boats.
fn request() -> PlanRequest {
    PlanRequest {
        race: "orc".into(),
        class: "warlock".into(),
        from_level: 1,
        to_level: 25,
        name: None,
        params: Some(
            overrides()
                .params
                .with_overrides(&["progression=cautious".to_owned()])
                .unwrap(),
        ),
        required_class_quests: None,
        professions: vec![],
        locale: None,
        mates: vec![],
        start: None,
    }
}

/// The guide, the editor that replays it and its timeline.
fn setup() -> &'static (PlanOutcome, Editor, Timeline) {
    static SETUP: OnceLock<(PlanOutcome, Editor, Timeline)> = OnceLock::new();
    SETUP.get_or_init(|| {
        let conn = database();
        let interrupt = job::Interrupt::default();
        interrupt.stop();
        let guide = job::plan_interruptible(&conn, &overrides(), &request(), &|_| {}, &interrupt, None).unwrap();
        let editor = Editor::open(&conn, &overrides(), &request(), None).unwrap();
        let timeline = editor.timeline(&guide, &Names::load(&conn, "enUS").unwrap());
        (guide, editor, timeline)
    })
}

/// The quests in the log after each step, rebuilt from the timeline.
fn logs(timeline: &Timeline) -> Vec<HashSet<i64>> {
    let mut log: HashSet<i64> = timeline.start.log.iter().copied().collect();
    timeline
        .steps
        .iter()
        .map(|s| {
            log.extend(&s.accepted);
            for q in &s.removed {
                log.remove(q);
            }
            log.clone()
        })
        .collect()
}

/// A step of the timeline per step of the guide, each one when the guide says: the last ends
/// with the guide.
#[test]
fn times_are_the_guides() {
    let (guide, _, timeline) = setup();
    let steps = &guide.route.steps;
    assert_eq!(timeline.steps.len(), steps.len());
    assert_eq!(timeline.matched, steps.len(), "the replay writes the guide again");
    for (k, (s, t)) in steps.iter().zip(&timeline.steps).enumerate() {
        assert!((t.end - s.time).abs() < 0.1, "step {k}: {} for {}", t.end, s.time);
        if k > 0 {
            assert!((t.start - steps[k - 1].time).abs() < 0.1, "step {k}");
        }
        let split = t.travel + t.combat + t.interact + t.wait;
        assert!(
            (split - (t.end - t.start)).abs() < 1.0,
            "step {k}: {split} in {}",
            t.end - t.start
        );
    }
    let last = timeline.steps.last().unwrap();
    assert!(
        (last.end - guide.route.total_time).abs() < 1.0,
        "{} vs {}",
        last.end,
        guide.route.total_time
    );
}

/// The quest is in the log once its accept step is done, and out of it once turned in.
#[test]
fn accepted_quests_are_in_the_log_until_turned_in() {
    let (guide, _, timeline) = setup();
    let logs = logs(timeline);
    let mut checked = 0;
    for (k, s) in guide.route.steps.iter().enumerate() {
        let Some(quest) = s.quest else { continue };
        match s.kind {
            StepKind::Accept => assert!(logs[k].contains(&quest), "step {k}: {quest} not in the log"),
            StepKind::TurnIn => assert!(!logs[k].contains(&quest), "step {k}: {quest} still in the log"),
            _ => continue,
        }
        checked += 1;
    }
    assert!(checked > 20);
    // The log never holds more than its size.
    assert!(logs.iter().all(|l| l.len() <= timeline.log_size));
    // What the log holds is named.
    let named: HashSet<i64> = timeline.quests.iter().map(|q| q.id).collect();
    assert!(logs.iter().flatten().all(|q| named.contains(q)));
}

/// Flights go from a flight path to another the character knows, and the character walks to
/// where each step is done: the moves before a step end there.
#[test]
fn the_character_moves_to_each_step() {
    let (guide, _, timeline) = setup();
    let nodes: Vec<[f64; 3]> = timeline.nodes.iter().map(|n| n.at).collect();
    let near = |a: [f64; 3], b: [f64; 3]| a[0] as i64 == b[0] as i64 && (a[1] - b[1]).hypot(a[2] - b[2]) < 30.0;
    let flights: Vec<_> = timeline.legs.iter().filter(|l| l.mode == LegMode::Flight).collect();
    assert!(!flights.is_empty(), "a guide to 25 flies");
    for f in &flights {
        assert!(
            nodes.iter().any(|&n| near(n, f.from)) && nodes.iter().any(|&n| near(n, f.to)),
            "{f:?}"
        );
        assert!(f.end > f.start, "{f:?}");
    }
    // One move after the other.
    for w in timeline.legs.windows(2) {
        assert!(w[1].start >= w[0].end - 0.05, "{:?} then {:?}", w[0], w[1]);
    }
    // Where the character is when a quest step is done: the last move before it ends there.
    let mut checked = 0;
    for (k, (s, t)) in guide.route.steps.iter().zip(&timeline.steps).enumerate() {
        let quest = matches!(s.kind, StepKind::Accept | StepKind::TurnIn);
        let (Some(at), true) = (s.world, quest) else { continue };
        let Some(last) = timeline.legs.iter().rev().find(|l| l.end <= t.end + 0.05) else {
            continue;
        };
        assert!(
            near(last.to, at),
            "step {k} ({}) at {at:?}, the character at {:?}",
            s.text,
            last.to
        );
        checked += 1;
    }
    assert!(checked > 20);
}

/// The walks go around the terrain: from where they start to where they end, on their
/// continent, never shorter than the straight line.
#[test]
fn walks_go_around_the_terrain() {
    let (_, _, timeline) = setup();
    let mut bent = 0;
    for leg in timeline
        .legs
        .iter()
        .filter(|l| matches!(l.mode, LegMode::Walk | LegMode::Mount))
    {
        if leg.via.is_empty() {
            continue;
        }
        bent += 1;
        let mut points = vec![[leg.from[1], leg.from[2]]];
        points.extend(leg.via.iter().map(|p| [f64::from(p[0]), f64::from(p[1])]));
        points.push([leg.to[1], leg.to[2]]);
        let length: f64 = points
            .windows(2)
            .map(|w| (w[0][0] - w[1][0]).hypot(w[0][1] - w[1][1]))
            .sum();
        let straight = (leg.from[1] - leg.to[1]).hypot(leg.from[2] - leg.to[2]);
        assert!(length >= straight - 0.5, "{length} < {straight}");
    }
    assert!(bent > 0, "some walks go around the terrain");
}
