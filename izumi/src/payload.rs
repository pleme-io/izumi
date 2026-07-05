//! The action-payload border — the second generic axis of the plane.
//!
//! An [`Item`](crate::Item)'s payload is *how to act on it*: mado carries a
//! [`SpawnSpec`](crate::SpawnSpec) (Enter spawns a session), another consumer
//! may carry a URL to open, a command to run, a typed follow-up. izumi only
//! requires the payload be plain typed data — cloneable, comparable (the
//! store's meaningful-change detection rides on `PartialEq`), thread-safe,
//! and serde-able (the warm-restart snapshot persists it verbatim).
//!
//! The blanket impl means any qualifying type IS a payload — no ceremony.

/// Plain typed data an [`Item`](crate::Item) carries as its action payload.
pub trait Payload:
    Clone
    + core::fmt::Debug
    + PartialEq
    + Send
    + Sync
    + serde::Serialize
    + serde::de::DeserializeOwned
    + 'static
{
}

impl<T> Payload for T where
    T: Clone
        + core::fmt::Debug
        + PartialEq
        + Send
        + Sync
        + serde::Serialize
        + serde::de::DeserializeOwned
        + 'static
{
}

#[cfg(test)]
mod tests {
    use super::Payload;

    fn assert_payload<A: Payload>() {}

    #[test]
    fn blanket_impl_covers_qualifying_types() {
        assert_payload::<crate::spawn::SpawnSpec>();
        assert_payload::<String>();
        assert_payload::<serde_json::Value>();
    }
}
