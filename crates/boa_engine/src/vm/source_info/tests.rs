use crate::vm::source_info::find_entry;

use super::Entry;

#[test]
fn find_empty() {
    let entries = &[];

    assert_eq!(find_entry(entries, 0), None);
}

#[test]
fn find_unit_ranges() {
    let entries = &[
        Entry {
            pc: 0,
            position: Some((1, 1).into()),
        },
        Entry {
            pc: 1,
            position: Some((2, 1).into()),
        },
        Entry {
            pc: 2,
            position: Some((3, 1).into()),
        },
        Entry {
            pc: 3,
            position: Some((4, 1).into()),
        },
    ];

    assert_eq!(find_entry(entries, 0), Some((1, 1).into()));
    assert_eq!(find_entry(entries, 1), Some((2, 1).into()));
    assert_eq!(find_entry(entries, 2), Some((3, 1).into()));
    assert_eq!(find_entry(entries, 3), Some((4, 1).into()));

    assert_eq!(find_entry(entries, 4), Some((4, 1).into()));
    assert_eq!(find_entry(entries, u32::MAX), Some((4, 1).into()));
}

#[test]
fn find_before_first_entry() {
    let entries = &[Entry {
        pc: 10,
        position: Some((1, 1).into()),
    }];

    assert_eq!(find_entry(entries, 0), None);
    assert_eq!(find_entry(entries, 5), None);
    assert_eq!(find_entry(entries, 10), Some((1, 1).into()));
}

#[test]
fn find_past_last_entry() {
    let entries = &[Entry {
        pc: 0,
        position: Some((1, 1).into()),
    }];

    assert_eq!(find_entry(entries, 0), Some((1, 1).into()));
    assert_eq!(find_entry(entries, 10), Some((1, 1).into()));
    assert_eq!(find_entry(entries, u32::MAX), Some((1, 1).into()));
}

#[test]
fn find_wide_spaced_ranges_odd() {
    let entries = &[
        Entry {
            pc: 0,
            position: Some((1, 1).into()),
        },
        Entry {
            pc: 10,
            position: Some((2, 1).into()),
        },
        Entry {
            pc: 20,
            position: Some((3, 1).into()),
        },
        Entry {
            pc: 30,
            position: Some((4, 1).into()),
        },
    ];

    assert_eq!(find_entry(entries, 0), Some((1, 1).into()));
    assert_eq!(find_entry(entries, 5), Some((1, 1).into()));
    assert_eq!(find_entry(entries, 9), Some((1, 1).into()));

    assert_eq!(find_entry(entries, 10), Some((2, 1).into()));
    assert_eq!(find_entry(entries, 15), Some((2, 1).into()));
    assert_eq!(find_entry(entries, 19), Some((2, 1).into()));

    assert_eq!(find_entry(entries, 20), Some((3, 1).into()));
    assert_eq!(find_entry(entries, 25), Some((3, 1).into()));
    assert_eq!(find_entry(entries, 29), Some((3, 1).into()));

    assert_eq!(find_entry(entries, 30), Some((4, 1).into()));
    assert_eq!(find_entry(entries, 35), Some((4, 1).into()));
    assert_eq!(find_entry(entries, 39), Some((4, 1).into()));
}

#[test]
fn find_wide_spaced_ranges_even() {
    let entries = &[
        Entry {
            pc: 0,
            position: Some((1, 1).into()),
        },
        Entry {
            pc: 10,
            position: Some((2, 1).into()),
        },
        Entry {
            pc: 20,
            position: Some((3, 1).into()),
        },
        Entry {
            pc: 30,
            position: Some((4, 1).into()),
        },
        Entry {
            pc: 40,
            position: Some((5, 1).into()),
        },
    ];

    assert_eq!(find_entry(entries, 0), Some((1, 1).into()));
    assert_eq!(find_entry(entries, 5), Some((1, 1).into()));
    assert_eq!(find_entry(entries, 9), Some((1, 1).into()));

    assert_eq!(find_entry(entries, 10), Some((2, 1).into()));
    assert_eq!(find_entry(entries, 15), Some((2, 1).into()));
    assert_eq!(find_entry(entries, 19), Some((2, 1).into()));

    assert_eq!(find_entry(entries, 20), Some((3, 1).into()));
    assert_eq!(find_entry(entries, 25), Some((3, 1).into()));
    assert_eq!(find_entry(entries, 29), Some((3, 1).into()));

    assert_eq!(find_entry(entries, 30), Some((4, 1).into()));
    assert_eq!(find_entry(entries, 35), Some((4, 1).into()));
    assert_eq!(find_entry(entries, 39), Some((4, 1).into()));

    assert_eq!(find_entry(entries, 40), Some((5, 1).into()));
    assert_eq!(find_entry(entries, 45), Some((5, 1).into()));
    assert_eq!(find_entry(entries, 49), Some((5, 1).into()));
}

#[test]
fn find_with_single_entry() {
    let entries = &[
        Entry {
            pc: 22,
            position: Some((1, 1).into()),
        },
        Entry {
            pc: 33,
            position: None,
        },
    ];

    assert_eq!(find_entry(entries, 0), None);
    assert_eq!(find_entry(entries, 10), None);
    assert_eq!(find_entry(entries, 21), None);

    assert_eq!(find_entry(entries, 22), Some((1, 1).into()));
    assert_eq!(find_entry(entries, 30), Some((1, 1).into()));
    assert_eq!(find_entry(entries, 32), Some((1, 1).into()));

    assert_eq!(find_entry(entries, 33), None);
    assert_eq!(find_entry(entries, u32::MAX), None);
}

#[test]
fn compact_map_matches_find_entry() {
    use super::{SourceMap, SourcePath};
    let mut seed = 0x2545_f491_u32;
    let mut rand = move |n: u32| {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed % n
    };
    for len in [0usize, 1, 2, 31, 32, 33, 64, 65, 200, 1000] {
        let mut pc = rand(4);
        let (mut line, mut col) = (1 + rand(50), 1 + rand(100_000));
        let entries: Vec<Entry> = (0..len)
            .map(|_| {
                pc += 1 + rand(40);
                let position = if rand(7) == 0 {
                    None
                } else {
                    line = (line as i64 + i64::from(rand(9)) - 4).max(1) as u32;
                    col = (col as i64 + i64::from(rand(4001)) - 2000).max(1) as u32;
                    Some((line, col).into())
                };
                Entry { pc, position }
            })
            .collect();
        let map = SourceMap::new(entries.clone().into_boxed_slice(), SourcePath::None);
        assert_eq!(map.entries(), entries);
        let max = entries.last().map_or(10, |e| e.pc + 10);
        for probe in 0..max {
            assert_eq!(map.find(probe), find_entry(&entries, probe), "len {len} pc {probe}");
        }
        assert_eq!(map.find(u32::MAX), find_entry(&entries, u32::MAX));
    }
}
