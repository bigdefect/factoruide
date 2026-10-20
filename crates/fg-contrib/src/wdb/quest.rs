//! Exact decoding of quest cache records with the layouts of the supported clients. A record is
//! read field by field and must end exactly at its last byte, so a layout that does not fit is
//! refused instead of giving shifted values. Checked on every record of the Classic Era (builds
//! 65300 and 70003), TBC Anniversary (70006) and Forever (70374) caches at hand, against the
//! quest database (objectives, levels, zones, start items, source items, flags).
//!
//! The fixed part is a run of 4-byte fields: id | type | quest level | ... | min level | quest
//! sort | ... | next quest in chain | XP difficulty | XP multiplier (f32) | ... | start item |
//! flags | ... | 4 x (reward item, amount, source item, source amount) | ... | accept sound kit |
//! complete sound kit | area group | time allowed (i64, seconds: MacKreel's Moonshine's 900 on
//! Classic Era) | objective count | allowable races (u64) | 2 treasure picker counts | ... | 2 conditional text counts | ...
//! The variable part holds 12 bytes of bit-packed string lengths (see `super::WIDTHS`), the
//! objectives and the strings, in an order that depends on the client:
//!   Classic Era and TBC (record version 11): lengths, objectives, strings, conditional texts.
//!   Forever (record version 12): reward display spells, objectives, conditional texts, lengths,
//!   strings.
//! Objective: u32 id | i32 type | i8 storage index | i32 object id | i32 amount | [i32 amount
//! for type 22] | u32 flags | u32 flags 2 | f32 progress bar weight | i32 visual effect count n |
//! [i32 parent objective] | n x i32 | u8 description length | [1 byte: visible bit] |
//! description. The bracketed fields are those of the newer builds (Classic Era 70003, TBC
//! Anniversary, Forever); older Classic Era builds (65300) have none of them.

use super::reader::{Reader, Result, WdbError, text};
use super::{CachedQuest, Objective, WIDTHS, XpReward};
use serde::{Deserialize, Serialize};

/// The most entries of a list the game sends in a quest (objectives, visual effects...).
const MAX_ENTRIES: usize = 32;

/// Positions of a layout's fields, as indexes of the 4-byte fields of the fixed part.
struct Layout {
    /// Record version of the cache file header.
    record_version: u32,
    /// Length of the fixed part, in fields.
    fixed: usize,
    min_level: usize,
    sort: usize,
    next_quest: usize,
    xp_difficulty: usize,
    start_item: usize,
    flags: usize,
    /// The 4 reward entries of 4 fields each.
    rewards: usize,
    objectives: usize,
    races: usize,
    /// Two counts of i32 arrays.
    treasure_pickers: usize,
    /// Two counts of conditional texts.
    conditional_texts: usize,
    /// Count of reward display spells (3 i32 each), before the objectives.
    display_spells: Option<usize>,
    /// Two counts of i32 arrays after the conditional texts.
    house: Option<usize>,
    /// String lengths before the objectives (else after them).
    lengths_first: bool,
    /// Objectives with the conditional amount, parent objective and visible bit.
    long_objectives: bool,
}

const LAYOUTS: [Layout; 3] = [
    // Classic Era, older builds (65300).
    Layout {
        record_version: 11,
        fixed: 120,
        min_level: 6,
        sort: 7,
        next_quest: 10,
        xp_difficulty: 11,
        start_item: 26,
        flags: 27,
        rewards: 31,
        objectives: 111,
        races: 112,
        treasure_pickers: 114,
        conditional_texts: 118,
        display_spells: None,
        house: None,
        lengths_first: true,
        long_objectives: false,
    },
    // Classic Era 70003 and TBC Anniversary 70006: one more reward field before the
    // start item, two more fields before the conditional text counts.
    Layout {
        record_version: 11,
        fixed: 123,
        min_level: 6,
        sort: 7,
        next_quest: 10,
        xp_difficulty: 11,
        start_item: 27,
        flags: 28,
        rewards: 32,
        objectives: 112,
        races: 113,
        treasure_pickers: 115,
        conditional_texts: 121,
        display_spells: None,
        house: None,
        lengths_first: true,
        long_objectives: true,
    },
    // Forever (70374).
    Layout {
        record_version: 12,
        fixed: 122,
        min_level: 4,
        sort: 6,
        next_quest: 9,
        xp_difficulty: 10,
        start_item: 24,
        flags: 25,
        rewards: 29,
        objectives: 109,
        races: 110,
        treasure_pickers: 112,
        conditional_texts: 118,
        display_spells: Some(16),
        house: Some(120),
        lengths_first: false,
        long_objectives: true,
    },
];

/// Quest fields beyond the title, objectives and XP, as the server sends them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestInfo {
    pub min_level: i32,
    /// QuestSortID: the zone (AreaTable id) when positive, minus the QuestSort id (class,
    /// profession, holiday...) when negative.
    pub sort: i32,
    /// The quest offered right after the turn-in. The server leaves it out of some chains the
    /// database has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_quest: Option<u32>,
    /// The item given at accept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_item: Option<u32>,
    /// Items needed by the quest that are not objectives (they drop only while on it).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub item_drops: Vec<ItemDrop>,
    /// Low 32 bits of the allowable races mask: all bits set (or 0x1FF on Classic Era) for any
    /// race. Forever sends any race for every quest seen so far.
    pub races: u32,
    /// QuestFlags.
    pub flags: u32,
    /// Timed quest: seconds from taking it to finish it, else it fails (absent from
    /// contributions of earlier app versions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_allowed: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemDrop {
    pub item: u32,
    pub count: i32,
}

/// A record read with a known layout.
pub struct Decoded {
    pub quest: CachedQuest,
    pub info: QuestInfo,
}

/// The record with the first layout that reads it exactly. `id` (the record's id in the file)
/// must then be the record's first field; `record_version` (of the file header) limits the
/// layouts tried.
pub fn decode(rec: &[u8], id: Option<u32>, record_version: Option<u32>) -> Option<Decoded> {
    LAYOUTS
        .iter()
        .filter(|l| record_version.is_none_or(|v| v == l.record_version))
        .find_map(|l| decode_with(rec, l, id).ok())
}

fn decode_with(rec: &[u8], l: &Layout, id: Option<u32>) -> Result<Decoded> {
    let mut fixed = Reader::new(rec);
    let fields = (0..l.fixed).map(|_| fixed.i32()).collect::<Result<Vec<i32>>>()?;
    let field = |i: usize| fields[i];
    if id.is_some_and(|id| field(0) as u32 != id) {
        return Err(WdbError::Invalid("quest id"));
    }
    let count = |i: usize, what| {
        usize::try_from(field(i))
            .ok()
            .filter(|n| *n <= MAX_ENTRIES)
            .ok_or(WdbError::Limit(what))
    };
    let objectives = count(l.objectives, "objectives")?;
    let pickers = count(l.treasure_pickers, "treasure pickers")? + count(l.treasure_pickers + 1, "treasure pickers")?;
    let texts = count(l.conditional_texts, "conditional texts")? + count(l.conditional_texts + 1, "conditional texts")?;

    let mut r = Reader::at(rec, l.fixed * 4)?;
    let (lengths, objectives) = if l.lengths_first {
        r.skip(4 * pickers)?;
        let lengths = lengths(&mut r)?;
        (lengths, read_objectives(&mut r, objectives, l.long_objectives)?)
    } else {
        if let Some(i) = l.display_spells {
            r.skip(12 * count(i, "display spells")?)?;
        }
        let objectives = read_objectives(&mut r, objectives, l.long_objectives)?;
        r.skip(4 * pickers)?;
        skip_conditional_texts(&mut r, texts)?;
        if let Some(i) = l.house {
            r.skip(4 * (count(i, "house rewards")? + count(i + 1, "house rewards")?))?;
        }
        (lengths(&mut r)?, objectives)
    };
    let mut strings = Vec::with_capacity(2);
    for (i, n) in lengths.into_iter().enumerate() {
        let s = text(r.take(n)?, "quest text")?;
        if i < 2 {
            strings.push(s);
        }
    }
    if l.lengths_first {
        skip_conditional_texts(&mut r, texts)?;
    }
    r.finish()?;
    let log = strings.pop().unwrap_or_default();
    let title = strings.pop().unwrap_or_default();
    if title.is_empty() {
        return Err(WdbError::Invalid("title"));
    }

    let positive = |i: usize| u32::try_from(field(i)).ok().filter(|v| *v > 0);
    let xp = u8::try_from(field(l.xp_difficulty))
        .ok()
        .filter(|d| *d <= 10)
        .zip(Some(f32::from_bits(field(l.xp_difficulty + 1) as u32)).filter(|m| (0.05..=20.0).contains(m)))
        .map(|(difficulty, multiplier)| XpReward {
            level: field(2),
            difficulty,
            multiplier,
        });
    let item_drops = (0..4)
        .filter_map(|i| {
            let at = l.rewards + 4 * i + 2;
            positive(at).map(|item| ItemDrop {
                item,
                count: field(at + 1),
            })
        })
        .collect();
    Ok(Decoded {
        quest: CachedQuest {
            title,
            log,
            objectives,
            confirmed: true,
            xp,
            info: None,
        },
        info: QuestInfo {
            min_level: field(l.min_level),
            sort: field(l.sort),
            next_quest: positive(l.next_quest),
            start_item: positive(l.start_item),
            item_drops,
            races: field(l.races) as u32,
            flags: field(l.flags) as u32,
            time_allowed: time_allowed(&fields, l.objectives),
        },
    })
}

/// The time allowed, the i64 right before the objective count: kept when it is a plausible
/// limit (up to a day; quests have 1 to 120 minutes).
fn time_allowed(fields: &[i32], objectives: usize) -> Option<u32> {
    let low = fields[objectives - 2] as u32;
    let high = fields[objectives - 1];
    (high == 0 && (1..=86_400).contains(&low)).then_some(low)
}

/// The 9 string lengths (12 bytes, the last bits are flags).
fn lengths(r: &mut Reader<'_>) -> Result<[usize; 9]> {
    let mut bits = super::reader::Bits::new(r.take(12)?);
    let mut out = [0; 9];
    for (len, width) in out.iter_mut().zip(WIDTHS) {
        *len = bits.read(width)? as usize;
    }
    Ok(out)
}

fn read_objectives(r: &mut Reader<'_>, n: usize, long: bool) -> Result<Vec<Objective>> {
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        r.skip(4)?; // id
        let kind = u8::try_from(r.i32()?).map_err(|_| WdbError::Invalid("objective type"))?;
        r.skip(1)?; // storage index
        let target = r.i32()?;
        let amount = r.i32()?;
        r.skip(if long { 16 } else { 12 })?; // [type 22 amount], flags, flags 2, weight
        let effects = r.count(MAX_ENTRIES, "visual effects")?;
        r.skip(if long { 4 } else { 0 } + 4 * effects)?; // [parent objective], effects
        let description = usize::from(r.u8()?);
        r.skip(usize::from(long) + description)?; // [visible bit], description
        out.push(Objective { kind, target, amount });
    }
    Ok(out)
}

/// Conditional texts: i32 condition | i32 quest giver | 12-bit length | text.
fn skip_conditional_texts(r: &mut Reader<'_>, n: usize) -> Result<()> {
    for _ in 0..n {
        r.skip(8)?;
        let len = r.take(2)?;
        r.skip(usize::from(u16::from_be_bytes([len[0], len[1]]) >> 4))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::reader::pack_bits;
    use super::*;

    const ID: i32 = 33;

    /// (type, object, amount, visual effects, description)
    type Obj<'a> = (i32, i32, i32, &'a [i32], &'a str);

    /// A record of a layout: fixed fields (id, objective count and `set`), the objectives, the
    /// title and log.
    fn record(l: &Layout, set: &[(usize, i32)], objectives: &[Obj<'_>], title: &str, log: &str) -> Vec<u8> {
        let mut fields = vec![0i32; l.fixed];
        fields[0] = ID;
        fields[l.objectives] = objectives.len() as i32;
        for &(i, v) in set {
            fields[i] = v;
        }
        let mut objs = Vec::new();
        for (i, &(kind, target, amount, effects, description)) in objectives.iter().enumerate() {
            objs.extend((380_016 + i as u32).to_le_bytes());
            objs.extend(kind.to_le_bytes());
            objs.push(0);
            objs.extend(target.to_le_bytes());
            objs.extend(amount.to_le_bytes());
            if l.long_objectives {
                objs.extend(0x6574_6164i32.to_le_bytes()); // unused amount: left as junk
            }
            objs.extend([0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]);
            objs.extend((effects.len() as i32).to_le_bytes());
            if l.long_objectives {
                objs.extend(0i32.to_le_bytes());
            }
            for e in effects {
                objs.extend(e.to_le_bytes());
            }
            objs.push(description.len() as u8);
            if l.long_objectives {
                objs.push(0x80);
            }
            objs.extend(description.bytes());
        }
        let mut lengths = vec![(title.len() as u32, 9), (log.len() as u32, 12)];
        lengths.extend(WIDTHS[2..].iter().map(|w| (0, *w)));
        let mut lengths = pack_bits(&lengths);
        lengths.resize(12, 0);
        let mut rec: Vec<u8> = fields.iter().flat_map(|f| f.to_le_bytes()).collect();
        if l.lengths_first {
            rec.extend(lengths);
            rec.extend(objs);
        } else {
            rec.extend(objs);
            rec.extend(lengths);
        }
        rec.extend(title.bytes().chain(log.bytes()));
        rec
    }

    fn sample(l: &Layout) -> Vec<u8> {
        let set = [
            (2, 12),
            (l.min_level, 8),
            (l.sort, 40),
            (l.next_quest, 103),
            (l.xp_difficulty, 5),
            (l.xp_difficulty + 1, 1.0f32.to_bits() as i32),
            (l.start_item, 1972),
            (l.flags, 8),
            (l.rewards, 2000),
            (l.rewards + 2, 1083),
            (l.rewards + 3, 1),
            (l.races, 77),
            (l.objectives - 2, 600),
        ];
        let objectives: [Obj<'_>; 2] = [(1, 750, 8, &[], ""), (0, 299, 10, &[42], "Vermin slain")];
        record(l, &set, &objectives, "Wolves Across the Border", "Bring 8 meat.")
    }

    #[test]
    fn decodes_every_layout_exactly() {
        for l in &LAYOUTS {
            let d = decode(&sample(l), Some(ID as u32), Some(l.record_version)).expect("decoded");
            assert_eq!(d.quest.title, "Wolves Across the Border");
            assert_eq!(d.quest.log, "Bring 8 meat.");
            assert!(d.quest.confirmed);
            let objectives: Vec<_> = d
                .quest
                .objectives
                .iter()
                .map(|o| (o.kind, o.target, o.amount))
                .collect();
            assert_eq!(objectives, [(1, 750, 8), (0, 299, 10)]);
            assert_eq!(
                d.quest.xp,
                Some(XpReward {
                    level: 12,
                    difficulty: 5,
                    multiplier: 1.0
                })
            );
            assert_eq!(
                d.info,
                QuestInfo {
                    min_level: 8,
                    sort: 40,
                    next_quest: Some(103),
                    start_item: Some(1972),
                    item_drops: vec![ItemDrop { item: 1083, count: 1 }],
                    races: 77,
                    flags: 8,
                    time_allowed: Some(600),
                }
            );
        }
    }

    #[test]
    fn checks_the_id_and_the_record_version() {
        let rec = sample(&LAYOUTS[2]);
        assert!(decode(&rec, Some(34), None).is_none());
        assert!(decode(&rec, None, Some(11)).is_none());
        assert!(decode(&rec, None, None).is_some());
    }

    #[test]
    fn rejects_truncated_and_trailing_records() {
        for l in &LAYOUTS {
            let rec = sample(l);
            for len in 0..rec.len() {
                assert!(decode(&rec[..len], None, None).is_none(), "decoded {len} bytes");
            }
            let mut longer = rec.clone();
            longer.push(b'.');
            assert!(decode(&longer, None, Some(l.record_version)).is_none());
        }
    }

    #[test]
    fn caps_counts() {
        let l = &LAYOUTS[1];
        let mut rec = sample(l);
        rec[l.objectives * 4..l.objectives * 4 + 4].copy_from_slice(&i32::MAX.to_le_bytes());
        assert!(matches!(decode_with(&rec, l, None), Err(WdbError::Limit("objectives"))));
        let effects: Vec<i32> = (0..=MAX_ENTRIES as i32).collect();
        let objectives: [Obj<'_>; 1] = [(0, 299, 10, &effects, "")];
        let rec = record(l, &[], &objectives, "Title", "Log");
        assert!(matches!(
            decode_with(&rec, l, None),
            Err(WdbError::Limit("visual effects"))
        ));
    }
}
