//! Inline spellchecking for requirement/test/result prose fields — a
//! `gui-ui`-only editing aid layered on top of the plain `String` buffers
//! `forms.rs` already owns. Never touches `syscalls`/`disk`/`logical`/
//! `gui-core`: a misspelling underline is not part of the validated data
//! model, so nothing here is persisted except the user's own custom-word
//! list (`GuiConfig::spellcheck_custom_words`).
//!
//! Engine: [`zspell`], a pure-Rust, Hunspell-compatible spellchecker. The
//! bundled dictionary (`assets/dictionaries/en_US.{aff,dic}`) is the first
//! embedded binary/text asset in this workspace — see
//! `assets/dictionaries/NOTICE.md` for its provenance and license (chosen
//! specifically for being permissively licensed with no copyleft terms).
//!
//! Dictionary construction happens on a background thread (`SpellChecker::
//! spawn_build`) and per-word suggestion lookups happen lazily on another
//! background thread per right-click (`SuggestionRequest::spawn`) — both
//! polled once per frame from `gui-ui`'s own event loop, never awaited or
//! blocked on, per this crate's "never block the render thread" rule.
//! Plain `std::thread`/`std::sync::mpsc`, not `tokio`: `gui-ui` has no
//! async runtime of its own (only `gui-core` does).

use std::collections::BTreeSet;
use std::fmt;
use std::ops::Range;
use std::sync::Arc;
use std::sync::mpsc::{self, TryRecvError};

const DICTIONARY_AFF: &str = include_str!("../assets/dictionaries/en_US.aff");
const DICTIONARY_DIC: &str = include_str!("../assets/dictionaries/en_US.dic");

/// The shared spellcheck engine handle — `NotStarted` until `GuiApp`'s
/// first real render frame kicks off the background build (see
/// `GuiApp::poll_spell_checker_build`; deliberately *not* started
/// eagerly in `GuiApp::new` itself, since that constructor runs for every
/// one of this crate's many logic-only unit tests too, most of which
/// never render a frame at all — building the real ~49k-word bundled
/// dictionary for every one of those would be pure waste, multiplied by
/// however many tests construct a `GuiApp`), then `Building` until that
/// thread reports back, `Unavailable` if the build ever fails (should not
/// happen with the bundled dictionary, but must degrade to "no
/// underlines, no popup" rather than panic). One instance lives on
/// `GuiApp` and is shared, by reference, across every form.
#[derive(Debug)]
pub enum SpellChecker {
    NotStarted,
    Building,
    Ready(Arc<zspell::Dictionary>),
    Unavailable,
}

impl SpellChecker {
    /// Starts building the bundled dictionary on a new background thread
    /// and returns a receiver for the finished `SpellChecker` — call once,
    /// at app startup, and poll the receiver each frame until it yields.
    pub fn spawn_build() -> mpsc::Receiver<SpellChecker> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = zspell::builder()
                .config_str(DICTIONARY_AFF)
                .dict_str(DICTIONARY_DIC)
                .build();
            let checker = match result {
                Ok(dict) => SpellChecker::Ready(Arc::new(dict)),
                Err(_) => SpellChecker::Unavailable,
            };
            // Nothing to do if the receiver's gone (app closed mid-build).
            let _ = tx.send(checker);
        });
        rx
    }

    /// Whether the dictionary has finished building — `view.rs` checks
    /// this before attaching any spellcheck rendering/popup at all, so a
    /// field behaves exactly as it did before this feature existed while
    /// `Building`/`Unavailable`.
    pub fn is_ready(&self) -> bool {
        matches!(self, SpellChecker::Ready(_))
    }

    fn dict(&self) -> Option<&zspell::Dictionary> {
        match self {
            SpellChecker::Ready(dict) => Some(dict),
            SpellChecker::NotStarted | SpellChecker::Building | SpellChecker::Unavailable => None,
        }
    }

    fn dict_arc(&self) -> Option<Arc<zspell::Dictionary>> {
        match self {
            SpellChecker::Ready(dict) => Some(Arc::clone(dict)),
            SpellChecker::NotStarted | SpellChecker::Building | SpellChecker::Unavailable => None,
        }
    }
}

/// One misspelled word found by a `FieldSpellCache::refresh` scan — a byte
/// range into the field's own text buffer plus the word itself (kept
/// alongside the range so a suggestion request doesn't need to re-slice
/// the buffer later).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Misspelling {
    pub range: Range<usize>,
    pub word: String,
}

/// A prose field's own memoized spellcheck results — one lives on each
/// free-text buffer in `forms.rs`. `refresh` is the only way to update it,
/// and is a no-op unless the text actually changed since the last refresh
/// that ran against a `Ready` dictionary, which is what keeps the actual
/// `zspell` scan off the per-frame render path.
#[derive(Debug, Clone, Default)]
pub struct FieldSpellCache {
    /// Empty (never matches a non-empty real buffer) until the first
    /// refresh that runs against a `Ready` dictionary — deliberately not
    /// updated while `checker` is still `Building`/`Unavailable`, so the
    /// moment the dictionary *does* become ready, the next `refresh` still
    /// sees a "changed" buffer and actually scans it, even if the field's
    /// text hasn't changed since the app started.
    checked_text: String,
    misspellings: Vec<Misspelling>,
}

impl FieldSpellCache {
    pub fn refresh(&mut self, text: &str, checker: &SpellChecker, custom_words: &BTreeSet<String>) {
        let Some(dict) = checker.dict() else {
            return;
        };
        if self.checked_text == text {
            return;
        }
        self.checked_text = text.to_owned();
        self.misspellings = dict
            .check_indices(text)
            .filter(|(_offset, word)| !custom_words.contains(*word))
            .map(|(offset, word)| Misspelling {
                range: offset..offset + word.len(),
                word: word.to_owned(),
            })
            .collect();
    }

    pub fn misspellings(&self) -> &[Misspelling] {
        &self.misspellings
    }

    /// Forces the next `refresh` to re-scan even if the buffer text hasn't
    /// changed — used after "Add to dictionary" so the just-accepted
    /// word's underline disappears on the very next frame instead of only
    /// once the user types something else.
    pub fn invalidate(&mut self) {
        self.checked_text.clear();
    }
}

/// Finds the misspelling (if any) whose byte range contains `offset` —
/// used to resolve a right-click's text-cursor position to the word it
/// landed on.
pub fn word_at_byte_offset(misspellings: &[Misspelling], offset: usize) -> Option<&Misspelling> {
    misspellings.iter().find(|m| m.range.contains(&offset))
}

/// Replaces `range` in `text` with `replacement` — the "click a
/// suggestion" action's actual string surgery, factored out mainly so a
/// replacement of different length than the original word has one
/// obviously-correct place to be tested.
pub fn replace_word_in_place(text: &mut String, range: &Range<usize>, replacement: &str) {
    text.replace_range(range.clone(), replacement);
}

/// A background suggestion lookup for one misspelled word, spawned on
/// right-click — never computed eagerly for a whole field, since `zspell`
/// documents its (feature-gated) suggestion algorithm as slow.
pub struct SuggestionRequest {
    word: String,
    rx: mpsc::Receiver<Vec<String>>,
}

impl fmt::Debug for SuggestionRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SuggestionRequest")
            .field("word", &self.word)
            .finish_non_exhaustive()
    }
}

/// The result of polling a `SuggestionRequest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuggestionPoll {
    Pending,
    Ready(Vec<String>),
}

impl SuggestionRequest {
    /// `None` if `checker` isn't `Ready` yet — nothing to spawn against.
    pub fn spawn(checker: &SpellChecker, word: String) -> Option<SuggestionRequest> {
        let dict = checker.dict_arc()?;
        let (tx, rx) = mpsc::channel();
        let thread_word = word.clone();
        std::thread::spawn(move || {
            // `suggest()` returns `None` only when the word is actually
            // correct, which shouldn't happen here since callers only ever
            // request suggestions for a word `check_indices` already
            // flagged — but treat it the same as "no suggestions" rather
            // than assuming it can't occur.
            let suggestions = dict
                .entry(&thread_word)
                .suggest()
                .map(|words| words.into_iter().map(str::to_owned).collect())
                .unwrap_or_default();
            let _ = tx.send(suggestions);
        });
        Some(SuggestionRequest { word, rx })
    }

    pub fn word(&self) -> &str {
        &self.word
    }

    pub fn poll(&self) -> SuggestionPoll {
        match self.rx.try_recv() {
            Ok(list) => SuggestionPoll::Ready(list),
            Err(TryRecvError::Empty) => SuggestionPoll::Pending,
            // The thread panicked or otherwise dropped the sender without
            // answering — treat the same as "no suggestions" rather than
            // leaving the popup stuck showing "Loading…" forever.
            Err(TryRecvError::Disconnected) => SuggestionPoll::Ready(Vec::new()),
        }
    }
}

/// Which prose field a `SpellPopupState` belongs to — one shared enum
/// across `RequirementFormState`/`TestFormState`/`ResultFormState` rather
/// than a per-form type, since `ResultFormState` only ever needs `Title`
/// and the others don't need to disambiguate anything it doesn't have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpellField {
    Title,
    RequirementText,
    RequirementGuidance,
    TestGuidance,
    TestText,
}

/// A right-click suggestion popup's transient state — `Some` on a form
/// while its popup is open, `None` otherwise. Only one can be open per
/// form at a time (right-clicking a different word replaces it).
#[derive(Debug)]
pub struct SpellPopupState {
    pub field: SpellField,
    pub misspelling: Misspelling,
    pub suggestions: SuggestionState,
}

#[derive(Debug)]
pub enum SuggestionState {
    Loading(SuggestionRequest),
    Ready(Vec<String>),
}

impl SpellPopupState {
    pub fn new(
        field: SpellField,
        misspelling: Misspelling,
        request: SuggestionRequest,
    ) -> SpellPopupState {
        SpellPopupState {
            field,
            misspelling,
            suggestions: SuggestionState::Loading(request),
        }
    }

    /// Promotes `Loading` to `Ready` once the background thread has
    /// answered — a no-op once already `Ready`, or while still `Loading`
    /// with nothing received yet. Called once per frame for whichever
    /// form currently has a popup open.
    pub fn poll(&mut self) {
        if let SuggestionState::Loading(request) = &self.suggestions {
            if let SuggestionPoll::Ready(list) = request.poll() {
                self.suggestions = SuggestionState::Ready(list);
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// A tiny inline Hunspell dictionary — not the bundled ~49k-word
    /// asset — so these tests stay fast and independent of the real
    /// dictionary's exact contents. `SET`/affix header is the minimum
    /// `zspell` needs to parse a config at all.
    const TEST_AFF: &str = "SET UTF-8\n";
    const TEST_DIC: &str = "4\npine\ncat\nthe\na\n";

    fn ready_checker() -> SpellChecker {
        let dict = zspell::builder()
            .config_str(TEST_AFF)
            .dict_str(TEST_DIC)
            .build()
            .expect("test fixture dictionary must build");
        SpellChecker::Ready(Arc::new(dict))
    }

    #[test]
    fn refresh_finds_a_misspelled_word() {
        let checker = ready_checker();
        let mut cache = FieldSpellCache::default();
        cache.refresh("the pine tre", &checker, &BTreeSet::new());
        assert_eq!(
            cache.misspellings(),
            &[Misspelling {
                range: 9..12,
                word: "tre".to_owned(),
            }]
        );
    }

    #[test]
    fn refresh_is_a_no_op_on_unchanged_text() {
        let checker = ready_checker();
        let mut cache = FieldSpellCache::default();
        cache.refresh("a tre", &checker, &BTreeSet::new());
        // Mutate the cached result directly so a real re-scan would be
        // observable, then refresh again with identical text.
        let sentinel = Misspelling {
            range: 0..1,
            word: "sentinel".to_owned(),
        };
        cache.misspellings = vec![sentinel.clone()];
        cache.refresh("a tre", &checker, &BTreeSet::new());
        assert_eq!(cache.misspellings(), &[sentinel]);
    }

    #[test]
    fn refresh_rescans_once_the_checker_becomes_ready() {
        let building = SpellChecker::Building;
        let mut cache = FieldSpellCache::default();
        cache.refresh("a tre", &building, &BTreeSet::new());
        assert!(cache.misspellings().is_empty());

        let checker = ready_checker();
        cache.refresh("a tre", &checker, &BTreeSet::new());
        assert_eq!(cache.misspellings().len(), 1);
        assert_eq!(cache.misspellings()[0].word, "tre");
    }

    #[test]
    fn custom_words_are_filtered_out() {
        let checker = ready_checker();
        let mut cache = FieldSpellCache::default();
        let custom = BTreeSet::from(["tre".to_owned()]);
        cache.refresh("a tre", &checker, &custom);
        assert!(cache.misspellings().is_empty());
    }

    #[test]
    fn invalidate_forces_a_rescan_of_unchanged_text() {
        let checker = ready_checker();
        let mut cache = FieldSpellCache::default();
        cache.refresh("a tre", &checker, &BTreeSet::new());
        assert_eq!(cache.misspellings().len(), 1);

        // Simulate "Add to dictionary" removing the word from future
        // scans, then invalidating so it takes effect immediately.
        let custom = BTreeSet::from(["tre".to_owned()]);
        cache.invalidate();
        cache.refresh("a tre", &checker, &custom);
        assert!(cache.misspellings().is_empty());
    }

    #[test]
    fn word_at_byte_offset_finds_containing_range_only() {
        let misspellings = [
            Misspelling {
                range: 2..5,
                word: "abc".to_owned(),
            },
            Misspelling {
                range: 10..13,
                word: "xyz".to_owned(),
            },
        ];
        assert_eq!(word_at_byte_offset(&misspellings, 0), None);
        assert_eq!(
            word_at_byte_offset(&misspellings, 2).map(|m| &m.word),
            Some(&"abc".to_owned())
        );
        assert_eq!(
            word_at_byte_offset(&misspellings, 4).map(|m| &m.word),
            Some(&"abc".to_owned())
        );
        assert_eq!(word_at_byte_offset(&misspellings, 5), None);
        assert_eq!(word_at_byte_offset(&misspellings, 9), None);
        assert_eq!(
            word_at_byte_offset(&misspellings, 10).map(|m| &m.word),
            Some(&"xyz".to_owned())
        );
    }

    #[test]
    fn replace_word_in_place_handles_different_length_replacements() {
        let mut text = "the pine tre stands".to_owned();
        replace_word_in_place(&mut text, &(9..12), "tree");
        assert_eq!(text, "the pine tree stands");

        let mut text = "the pine tree stands".to_owned();
        replace_word_in_place(&mut text, &(9..13), "oak");
        assert_eq!(text, "the pine oak stands");
    }

    #[test]
    fn suggestion_request_delivers_over_the_background_thread() {
        let checker = ready_checker();
        let request =
            SuggestionRequest::spawn(&checker, "tre".to_owned()).expect("checker is ready");
        assert_eq!(request.word(), "tre");

        let mut attempts = 0;
        let suggestions = loop {
            match request.poll() {
                SuggestionPoll::Ready(list) => break list,
                SuggestionPoll::Pending => {
                    attempts += 1;
                    // A real wall-clock bound (up to ~5s), not a spin-count
                    // one — under no contention `yield_now()` returns
                    // almost instantly, so a bare iteration cap doesn't
                    // actually bound how long this waits for genuine work
                    // (parsing/Levenshtein-scanning the fixture dictionary)
                    // to finish on the other thread.
                    assert!(attempts < 5000, "suggestion thread never answered");
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
        };
        // Not asserting on suggestion *quality* (that's zspell's own
        // unstable algorithm) — only that the plumbing actually delivers
        // something for a word one Levenshtein edit from a real entry.
        assert!(!suggestions.is_empty());
    }

    #[test]
    fn suggestion_request_is_none_when_checker_is_not_ready() {
        assert!(SuggestionRequest::spawn(&SpellChecker::Building, "tre".to_owned()).is_none());
        assert!(SuggestionRequest::spawn(&SpellChecker::Unavailable, "tre".to_owned()).is_none());
    }

    #[test]
    fn spawn_build_delivers_a_ready_checker() {
        let rx = SpellChecker::spawn_build();
        let mut attempts = 0;
        let checker = loop {
            match rx.try_recv() {
                Ok(checker) => break checker,
                Err(TryRecvError::Empty) => {
                    attempts += 1;
                    // Same "real wall-clock bound" reasoning as above —
                    // building the full bundled ~49k-word dictionary is
                    // genuine work, not something a bare spin-count should
                    // race against.
                    assert!(attempts < 5000, "dictionary build never completed");
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(TryRecvError::Disconnected) => panic!("build thread dropped without sending"),
            }
        };
        assert!(matches!(checker, SpellChecker::Ready(_)));
    }
}
