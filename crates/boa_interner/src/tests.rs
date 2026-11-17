use crate::{COMMON_STRINGS_UTF8, COMMON_STRINGS_UTF16, Interner, Sym};
use boa_macros::utf16;

#[track_caller]
fn sym_from_usize(index: usize) -> Sym {
    Sym::new(index).expect("Invalid NonZeroUsize")
}

#[test]
fn check_static_strings() {
    let mut interner = Interner::default();

    for (i, &str) in COMMON_STRINGS_UTF8.into_iter().enumerate() {
        assert_eq!(interner.get_or_intern(str), sym_from_usize(i + 1));
    }
}

#[test]
fn check_new_string() {
    let mut interner = Interner::default();

    assert!(interner.get_or_intern("my test string").get() > COMMON_STRINGS_UTF8.len());
}

#[test]
fn check_resolve() {
    let mut interner = Interner::default();

    let utf_8_strings = ["test string", "arguments", "hello"];
    let utf_8_strings = utf_8_strings.into_iter();
    let utf_16_strings = [utf16!("TEST STRING"), utf16!("ARGUMENTS"), utf16!("HELLO")];
    let utf_16_strings = utf_16_strings.into_iter();

    for (s8, s16) in utf_8_strings.zip(utf_16_strings) {
        let sym = interner.get_or_intern(s8);
        let resolved = interner.resolve(sym).unwrap();
        assert_eq!(Some(s8), resolved.utf8());
        let new_sym = interner.get_or_intern(s8);
        assert_eq!(sym, new_sym);

        let sym = interner.get_or_intern(s16);
        let resolved = interner.resolve(sym).unwrap();
        assert_eq!(s16, resolved.utf16());
        let new_sym = interner.get_or_intern(s16);
        assert_eq!(sym, new_sym);
    }
}

#[test]
fn check_static_resolve() {
    let mut interner = Interner::default();

    for (utf8, utf16) in COMMON_STRINGS_UTF8
        .into_iter()
        .copied()
        .zip(COMMON_STRINGS_UTF16.iter().copied())
        .chain([
            ("my test str", utf16!("my test str")),
            ("hello world", utf16!("hello world")),
            (";", utf16!(";")),
        ])
    {
        let sym = interner.get_or_intern_static(utf8, utf16);
        let resolved = interner.resolve(sym).unwrap();
        assert_eq!(Some(utf8), resolved.utf8());
        assert_eq!(utf16, resolved.utf16());

        let new_sym = interner.get_or_intern(utf8);

        assert_eq!(sym, new_sym);
    }
}

#[test]
fn check_unpaired_surrogates() {
    let mut interner = Interner::default();

    let unp = &[
        0xDC15u16, 0xDC19, 'h' as u16, 'e' as u16, 'l' as u16, 'l' as u16, 'o' as u16,
    ];
    let unp2 = &[
        0xDC01u16, 'w' as u16, 'o' as u16, 'r' as u16, 0xDCF4, 'l' as u16, 'd' as u16,
    ];

    let sym = interner.get_or_intern("abc");
    let sym2 = interner.get_or_intern("def");

    let sym3 = interner.get_or_intern(unp);
    let sym4 = interner.get_or_intern(utf16!("ghi"));
    let sym5 = interner.get_or_intern(unp2);

    let sym6 = interner.get_or_intern("jkl");

    assert_eq!(interner.resolve_expect(sym).utf8(), Some("abc"));
    assert_eq!(interner.resolve_expect(sym).utf16(), utf16!("abc"));

    assert_eq!(interner.resolve_expect(sym2).utf8(), Some("def"));
    assert_eq!(interner.resolve_expect(sym2).utf16(), utf16!("def"));

    assert!(interner.resolve_expect(sym3).utf8().is_none());
    assert_eq!(interner.resolve_expect(sym3).utf16(), unp);

    assert_eq!(interner.resolve_expect(sym4).utf8(), Some("ghi"));
    assert_eq!(interner.resolve_expect(sym4).utf16(), utf16!("ghi"));

    assert!(interner.resolve_expect(sym5).utf8().is_none());
    assert_eq!(interner.resolve_expect(sym5).utf16(), unp2);

    assert_eq!(interner.resolve_expect(sym6).utf8(), Some("jkl"));
    assert_eq!(interner.resolve_expect(sym6).utf16(), utf16!("jkl"));
}

#[test]
fn check_empty_interner() {
    let interner = Interner::default();

    let sym = sym_from_usize(123); // Choose an arbitrary symbol

    assert!(interner.resolve(sym).is_none());
}

#[test]
fn check_capacity() {
    let interner = Interner::with_capacity(100);

    let sym = sym_from_usize(123); // Choose an arbitrary symbol

    assert!(interner.resolve(sym).is_none());
}

#[test]
fn check_is_latin1() {
    static STATIC_STR: &str = "static_latin1";
    static STATIC_UTF16: &[u16] = &[
        's' as u16, 't' as u16, 'a' as u16, 't' as u16, 'i' as u16, 'c' as u16, '_' as u16,
        'l' as u16, 'a' as u16, 't' as u16, 'i' as u16, 'n' as u16, '1' as u16,
    ];

    let mut interner = Interner::default();

    // Common/static strings (e.g. keywords) are always Latin1.
    let common_sym = interner.get_or_intern("break");
    assert!(interner.is_latin1(common_sym));

    // Dynamic ASCII string.
    let ascii_sym = interner.get_or_intern("hello_world");
    assert!(interner.is_latin1(ascii_sym));

    // Dynamic non-ASCII but Latin1-encodable (U+0080..=U+00FF).
    let latin1_sym = interner.get_or_intern(&[0x00E9u16, 0x00FC, 0x00F1][..]);
    assert!(interner.is_latin1(latin1_sym));

    // Dynamic non-Latin1 (code unit > 0xFF).
    let non_latin1_sym = interner.get_or_intern(&[0x4E2Du16, 0x6587][..]);
    assert!(!interner.is_latin1(non_latin1_sym));

    // Boundary: U+00FF (last Latin1) and U+0100 (first non-Latin1).
    let boundary_sym = interner.get_or_intern(&[0x00FFu16][..]);
    assert!(interner.is_latin1(boundary_sym));
    let boundary_non_sym = interner.get_or_intern(&[0x0100u16][..]);
    assert!(!interner.is_latin1(boundary_non_sym));

    // get_or_intern_static also caches correctly.
    let static_sym = interner.get_or_intern_static(STATIC_STR, STATIC_UTF16);
    assert!(interner.is_latin1(static_sym));
}

#[test]
fn truncate_releases_and_recycles() {
    let mut interner = Interner::default();
    let kept = interner.get_or_intern("kept_string");
    let cp = interner.checkpoint();
    let dead = interner.get_or_intern("dead_string");
    let dead2 = interner.get_or_intern("dead_string_two");
    assert_eq!(interner.dynamic_len(), 3);

    interner.truncate(&cp);
    assert_eq!(interner.dynamic_len(), 1);
    assert_eq!(interner.resolve_expect(kept).utf8(), Some("kept_string"));
    assert!(interner.resolve(dead).is_none());
    assert!(interner.resolve(dead2).is_none());
    assert!(interner.get("dead_string").is_none());
    assert_eq!(interner.get("kept_string"), Some(kept));

    // Recycling reuses indices deterministically: the next intern gets
    // the first freed slot, and resolution follows the new string.
    let recycled = interner.get_or_intern("recycled_string");
    assert_eq!(recycled, dead);
    assert_eq!(
        interner.resolve_expect(recycled).utf8(),
        Some("recycled_string")
    );
}

#[test]
fn truncate_across_slab_roll() {
    let mut interner = Interner::default();
    let kept = interner.get_or_intern("small_kept");
    let cp = interner.checkpoint();
    // Big strings force head rolls into `full` past the checkpoint, so
    // truncate must restore the checkpoint-era head from `full`.
    let mut dead_syms = alloc::vec::Vec::new();
    for i in 0..64 {
        let big = alloc::format!("{}{}", "x".repeat(1024), i);
        dead_syms.push(interner.get_or_intern(big.as_str()));
    }
    interner.truncate(&cp);
    assert_eq!(interner.resolve_expect(kept).utf8(), Some("small_kept"));
    for s in dead_syms {
        assert!(interner.resolve(s).is_none());
    }
    // The surviving prefix must still be appendable and resolvable.
    let after = interner.get_or_intern("post_roll");
    assert_eq!(interner.resolve_expect(after).utf8(), Some("post_roll"));
    assert_eq!(interner.resolve_expect(kept).utf8(), Some("small_kept"));
}

#[test]
fn truncate_utf16_sentinel_dedup() {
    let mut interner = Interner::default();
    // Unpaired surrogates are utf16-only: each interns the `""` sentinel
    // into the utf8 side, exercising the duplicate-key cache rebuild.
    let kept_lone = interner.get_or_intern(&[0xD800u16][..]);
    let cp = interner.checkpoint();
    let dead_lone = interner.get_or_intern(&[0xDC00u16][..]);
    interner.truncate(&cp);
    assert_eq!(interner.resolve_expect(kept_lone).utf16(), &[0xD800u16][..]);
    assert!(interner.resolve(dead_lone).is_none());
    // The surviving sentinel must still dedup new surrogate interns.
    let again = interner.get_or_intern(&[0xDC00u16][..]);
    assert_eq!(interner.resolve_expect(again).utf16(), &[0xDC00u16][..]);
    assert_eq!(interner.get_or_intern(&[0xD800u16][..]), kept_lone);
}

#[test]
fn truncate_noop_and_statics() {
    let mut interner = Interner::default();
    let cp = interner.checkpoint();
    interner.truncate(&cp);
    assert_eq!(interner.dynamic_len(), 0);
    // Static common strings live outside the dynamic tables and always
    // survive truncation with stable symbols.
    let arguments = interner.get_or_intern("arguments");
    interner.truncate(&cp);
    assert_eq!(interner.get_or_intern("arguments"), arguments);
    assert_eq!(interner.resolve_expect(arguments).utf8(), Some("arguments"));
}

#[test]
#[should_panic(expected = "checkpoint belongs to another interner")]
fn truncate_rejects_foreign_checkpoint() {
    let mut owner = Interner::with_capacity(256);
    for index in 0..16 {
        let short = alloc::format!("owner-{index:02}");
        owner.get_or_intern(short.as_str());
    }
    let checkpoint = owner.checkpoint();

    let mut other = Interner::default();
    // The foreign checkpoint keeps spans whose slabs it would free.
    for index in 0..17 {
        let long = alloc::format!("{}-{index}", "x".repeat(4096));
        other.get_or_intern(long.as_str());
    }
    other.truncate(&checkpoint);
}
