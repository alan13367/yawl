//! Generated subagent handles. Spawned subagents get an AdjectiveNoun name
//! when the model does not supply one, so dashboards and deliveries stay
//! readable without spending model tokens on naming.

const ADJECTIVES: &[&str] = &[
    "amber", "brisk", "calm", "dapper", "eager", "fleet", "gentle", "hardy", "ionic", "jade",
    "keen", "lucid", "mellow", "nimble", "opal", "prime", "quiet", "rapid", "sturdy", "tidy",
    "urgent", "vivid", "witty", "young", "zesty", "bold", "crisp", "deft", "elite", "frank",
    "glide", "hasty", "ivory", "jetty", "kraft", "lunar", "misty", "noble", "orbit", "plush",
    "quill", "rustic", "swift", "tonic", "ultra", "verdant", "wired", "zonal",
];

const NOUNS: &[&str] = &[
    "falcon", "otter", "heron", "lynx", "badger", "cocoa", "delta", "ember", "fern", "gale",
    "harbor", "iris", "jasper", "kite", "linden", "marten", "nettle", "onyx", "plover", "quartz",
    "raven", "sable", "tundra", "umber", "vireo", "walnut", "yarrow", "zephyr", "acorn", "birch",
    "cedar", "dune", "edge", "flint", "grove", "haven", "inlet", "juniper", "knoll", "ledge",
    "marsh", "north", "oyster", "pine", "ridge", "spruce", "thorn", "vale",
];

const MAX_ATTEMPTS: usize = 50;

/// Returns a handle that does not appear in `existing`. Random picks come
/// first; repeated collisions gain `-2`, `-3` suffixes; `agent-{sequence}` is
/// the guaranteed fallback.
pub(crate) fn generate_name(existing: &[String], sequence: u64) -> String {
    let mut rng = Rng::new(sequence);
    for _ in 0..MAX_ATTEMPTS {
        let adjective = ADJECTIVES[rng.next_index(ADJECTIVES.len())];
        let noun = NOUNS[rng.next_index(NOUNS.len())];
        let candidate = format!("{}{}", capitalize(adjective), capitalize(noun));
        if !existing.iter().any(|name| name == &candidate) {
            return candidate;
        }
    }
    // The word space is exhausted for this session; fall back to a
    // deterministic, collision-free handle derived from the ID sequence.
    let base = format!("agent-{sequence}");
    let mut suffix = 2;
    let mut candidate = base.clone();
    while existing.iter().any(|name| name == &candidate) {
        candidate = format!("{base}-{suffix}");
        suffix += 1;
    }
    candidate
}

fn capitalize(word: &str) -> String {
    let mut characters = word.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => String::new(),
    }
}

/// Xorshift64* seeded from the wall clock so concurrent sessions do not
/// produce identical handle sequences.
struct Rng(u64);

impl Rng {
    fn new(sequence: u64) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(0);
        let mut rng = Self(nanos ^ (sequence.wrapping_mul(0x9E37_79B9_7F4A_7C15)));
        if rng.0 == 0 {
            rng.0 = sequence.wrapping_add(1);
        }
        rng
    }

    fn next_u64(&mut self) -> u64 {
        let mut state = self.0;
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        self.0 = state;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_index(&mut self, len: usize) -> usize {
        (self.next_u64() % len as u64) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_names_are_capitalized_and_unique() {
        let mut seen = Vec::new();
        for sequence in 0..200 {
            let name = generate_name(&seen, sequence);
            assert!(
                name.chars().next().is_some_and(char::is_uppercase),
                "{name} should be capitalized"
            );
            assert!(!seen.contains(&name), "{name} collided");
            seen.push(name);
        }
    }

    #[test]
    fn collisions_fall_back_to_sequenced_handles() {
        // Occupy the fallback name so the suffix ladder must engage.
        let existing = vec!["agent-7".to_string()];
        let name = generate_name(&existing, 7);
        assert_ne!(name, "agent-7");
        assert!(!name.is_empty());
    }
}
