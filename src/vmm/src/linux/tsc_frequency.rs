//! Preserve the KVM snapshot's kHz encoding while using Hz in memory.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub(super) fn to_khz(hz: u64) -> Result<u32, &'static str> {
    if !hz.is_multiple_of(1000) {
        return Err("TSC frequency is not an exact number of kHz");
    }
    u32::try_from(hz / 1000).map_err(|_| "TSC frequency exceeds KVM's kHz range")
}

pub(super) fn serialize<S: Serializer>(hz: &u64, serializer: S) -> Result<S::Ok, S::Error> {
    to_khz(*hz)
        .map_err(serde::ser::Error::custom)?
        .serialize(serializer)
}

pub(super) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    u32::deserialize(deserializer).map(|khz| u64::from(khz) * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize, Deserialize)]
    struct SavedClock {
        #[serde(with = "super")]
        tsc_frequency_hz: u64,
    }

    #[test]
    fn existing_khz_snapshot_bytes_remain_compatible() {
        // Existing bincode u32 encoding of 2,400,000 kHz, not a new Hz encoding.
        let legacy = [252, 0, 159, 36, 0];
        let config = bincode::config::standard();
        let (state, used): (SavedClock, usize) =
            bincode::serde::decode_from_slice(&legacy, config).unwrap();
        assert_eq!(used, legacy.len());
        assert_eq!(state.tsc_frequency_hz, 2_400_000_000);
        assert_eq!(
            bincode::serde::encode_to_vec(state, config).unwrap(),
            legacy
        );
    }

    #[test]
    fn unrepresentable_frequencies_are_not_rounded_or_truncated() {
        for tsc_frequency_hz in [2_400_000_001, (u64::from(u32::MAX) + 1) * 1000] {
            assert!(bincode::serde::encode_to_vec(
                SavedClock { tsc_frequency_hz },
                bincode::config::standard(),
            )
            .is_err());
        }
    }
}
