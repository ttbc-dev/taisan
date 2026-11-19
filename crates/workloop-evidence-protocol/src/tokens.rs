//! Opaque page-continuation and checkpoint tokens for `rui-evidence/1`.
//!
//! The wire contract treats `--after` checkpoints and `--cursor` page
//! cursors as opaque strings; this module is the producer-side codec
//! behind them. A token binds the issuing installation and store epoch,
//! the snapshot watermark, the retained-history boundary, and the next
//! stable stream position. It is self-describing by design: these
//! commands already run inside the installation's own trust boundary,
//! so a checksum detects corruption while a forged token only grants
//! data the caller could already read.

use serde::{Deserialize, Serialize};

use crate::{ErrorCode, PROTOCOL, sha256};

/// How long an unfinished page sequence may be resumed by `--cursor`
/// before the producer reports `snapshot_expired`. The contract
/// requires at least ten minutes; the producer grants one hour.
pub const SNAPSHOT_TTL_SECONDS: i64 = 3600;

const TOKEN_PREFIX: &str = "ev1.";

const BASE64URL: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// A structured producer error mapped onto the frozen reason codes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportError {
    pub code: ErrorCode,
    pub message: String,
}

impl ExportError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for ExportError {}

fn invalid(message: &str) -> ExportError {
    ExportError::new(ErrorCode::InvalidCursor, message)
}

/// Which opaque token a caller must present for each request mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// `next_cursor` continuation for an unfinished page sequence.
    Cursor,
    /// `checkpoint` for a new export after a committed watermark.
    Checkpoint,
}

/// The decoded binding an opaque token carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportToken {
    pub kind: TokenKind,
    pub installation_id: String,
    pub store_epoch: String,
    /// Upper watermark of the sequence this token belongs to.
    pub watermark: u64,
    /// Retained-history boundary observed when the sequence opened.
    pub retained_from_seq: u64,
    /// Lower checkpoint the owning sequence exported after; `None` for
    /// a full retained-history export.
    pub after_seq: Option<u64>,
    /// Stable snapshot identity shared by every page of the sequence.
    pub snapshot_id: String,
    /// Next stable stream position (merged-order record index). Only
    /// meaningful on `Cursor` tokens.
    pub position: u64,
    /// Next orphan-gap queue index accompanying `position`. Only
    /// meaningful on `Cursor` tokens.
    pub gap_position: u64,
    /// Seconds since the Unix epoch when the sequence opened; feeds the
    /// snapshot TTL.
    pub minted_unix_seconds: i64,
}

#[derive(Clone, Serialize, Deserialize)]
struct Wire {
    k: String,
    i: String,
    e: String,
    w: u64,
    r: u64,
    a: Option<u64>,
    s: String,
    p: u64,
    g: u64,
    m: i64,
    c: String,
}

impl ExportToken {
    fn wire(&self) -> Wire {
        Wire {
            k: match self.kind {
                TokenKind::Cursor => "cursor".into(),
                TokenKind::Checkpoint => "checkpoint".into(),
            },
            i: self.installation_id.clone(),
            e: self.store_epoch.clone(),
            w: self.watermark,
            r: self.retained_from_seq,
            a: self.after_seq,
            s: self.snapshot_id.clone(),
            p: self.position,
            g: self.gap_position,
            m: self.minted_unix_seconds,
            c: String::new(),
        }
    }

    /// The opaque string the contract moves between calls.
    pub fn encode(&self) -> String {
        let mut wire = self.wire();
        let unsigned = serde_json::to_string(&wire).expect("token fields serialize");
        wire.c = checksum(&unsigned);
        let signed = serde_json::to_string(&wire).expect("token fields serialize");
        format!("{TOKEN_PREFIX}{}", base64url(signed.as_bytes()))
    }

    /// Parse and integrity-check an opaque token. A malformed or
    /// corrupted string is `invalid_cursor`, never a panic.
    pub fn decode(raw: &str) -> Result<Self, ExportError> {
        let body = raw
            .strip_prefix(TOKEN_PREFIX)
            .ok_or_else(|| invalid("unrecognized token form"))?;
        let bytes = unbase64url(body).ok_or_else(|| invalid("corrupt token encoding"))?;
        let wire: Wire =
            serde_json::from_slice(&bytes).map_err(|_| invalid("corrupt token payload"))?;
        let unsigned = serde_json::to_string(&Wire {
            c: String::new(),
            ..wire.clone()
        })
        .map_err(|_| invalid("corrupt token payload"))?;
        if checksum(&unsigned) != wire.c {
            return Err(invalid("token checksum mismatch"));
        }
        let kind = match wire.k.as_str() {
            "cursor" => TokenKind::Cursor,
            "checkpoint" => TokenKind::Checkpoint,
            _ => return Err(invalid("unrecognized token kind")),
        };
        if wire.i.is_empty() || wire.e.is_empty() {
            return Err(invalid("token binding is incomplete"));
        }
        Ok(Self {
            kind,
            installation_id: wire.i,
            store_epoch: wire.e,
            watermark: wire.w,
            retained_from_seq: wire.r,
            after_seq: wire.a,
            snapshot_id: wire.s,
            position: wire.p,
            gap_position: wire.g,
            minted_unix_seconds: wire.m,
        })
    }

    /// Rebind checks for a continuation: the token must be the kind the
    /// caller needs, belong to this installation and epoch, still sit
    /// inside retained history, still be within the snapshot TTL, and
    /// not pin a watermark beyond the store's current history.
    pub fn check_binding(
        &self,
        expected: TokenKind,
        installation_id: &str,
        store_epoch: &str,
        retained_from_seq: u64,
        current_watermark: u64,
        now_unix_seconds: i64,
    ) -> Result<(), ExportError> {
        if self.kind != expected {
            return Err(invalid(
                "token kind does not match the flag it was passed to",
            ));
        }
        if self.installation_id != installation_id {
            return Err(ExportError::new(
                ErrorCode::InstallationMismatch,
                "token was issued by a different installation",
            ));
        }
        if self.store_epoch != store_epoch {
            return Err(ExportError::new(
                ErrorCode::EpochMismatch,
                "token was issued under a different store epoch",
            ));
        }
        // A token pinned above the store's current watermark names
        // history that no longer exists (restore or rewind): the
        // sequence can never complete, so it expires rather than
        // serving a stale page or an empty delta.
        if self.watermark > current_watermark {
            return Err(ExportError::new(
                match expected {
                    TokenKind::Cursor => ErrorCode::SnapshotExpired,
                    TokenKind::Checkpoint => ErrorCode::CheckpointExpired,
                },
                "the token's pinned watermark is beyond the store's current history",
            ));
        }
        match expected {
            TokenKind::Cursor => {
                if self.after_seq.is_some_and(|a| a > self.watermark) {
                    return Err(invalid("token checkpoint exceeds its snapshot watermark"));
                }
                if self.retained_from_seq < retained_from_seq || self.watermark < retained_from_seq
                {
                    return Err(ExportError::new(
                        ErrorCode::SnapshotExpired,
                        "snapshot history is no longer retained",
                    ));
                }
                if now_unix_seconds.saturating_sub(self.minted_unix_seconds) > SNAPSHOT_TTL_SECONDS
                {
                    return Err(ExportError::new(
                        ErrorCode::SnapshotExpired,
                        "snapshot lifetime expired; restart from the last committed checkpoint",
                    ));
                }
            }
            TokenKind::Checkpoint => {
                // The pinned lower boundary must still be retained:
                // replaying from a later boundary renumbers revisions
                // the consumer already committed, which the contract
                // answers with checkpoint_expired, not a corrupt delta.
                if self.retained_from_seq < retained_from_seq || self.watermark < retained_from_seq
                {
                    return Err(ExportError::new(
                        ErrorCode::CheckpointExpired,
                        "the checkpoint's pinned history is no longer retained",
                    ));
                }
            }
        }
        Ok(())
    }
}

fn checksum(unsigned_json: &str) -> String {
    sha256(unsigned_json.as_bytes())[..16].to_string()
}

/// The stable snapshot identity one open page sequence shares. The id
/// binds the snapshot's identity material plus a per-sequence nonce:
/// the contract requires every new export sequence to carry a fresh
/// snapshot id, so two polls at the same watermark must not collide —
/// only pages of one sequence (via the cursor token) reuse it.
pub fn snapshot_id(
    installation_id: &str,
    store_epoch: &str,
    after_seq: Option<u64>,
    watermark: u64,
    nonce: &str,
) -> String {
    let material = format!(
        "{PROTOCOL}|{installation_id}|{store_epoch}|{}|{watermark}|{nonce}",
        after_seq
            .map(|s| s.to_string())
            .unwrap_or_else(|| "full".into())
    );
    format!("ev-snap-{}", &sha256(material.as_bytes())[..16])
}

fn base64url(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | ((*b as u32) << (16 - 8 * i)));
        let take = chunk.len() + 1;
        for s in 0..take {
            out.push(BASE64URL[(n >> (18 - 6 * s)) as usize & 0x3f] as char);
        }
    }
    out
}

fn unbase64url(s: &str) -> Option<Vec<u8>> {
    let mut table = [0xffu8; 256];
    for (i, b) in BASE64URL.iter().enumerate() {
        table[*b as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for b in s.bytes() {
        let v = table[b as usize];
        if v == 0xff {
            return None;
        }
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(kind: TokenKind) -> ExportToken {
        ExportToken {
            kind,
            installation_id: "inst-1".into(),
            store_epoch: "10".into(),
            watermark: 42,
            retained_from_seq: 1,
            after_seq: Some(7),
            snapshot_id: "snap-x".into(),
            position: 5,
            gap_position: 0,
            minted_unix_seconds: 1000,
        }
    }

    #[test]
    fn round_trip() {
        let t = token(TokenKind::Cursor);
        assert_eq!(ExportToken::decode(&t.encode()), Ok(t));
    }

    #[test]
    fn corruption_is_invalid_cursor() {
        let mut s = token(TokenKind::Cursor).encode();
        let i = s.len() / 2;
        let c = s.as_bytes()[i];
        let flip = if c == b'A' { 'B' } else { 'A' };
        s.replace_range(i..i + 1, &flip.to_string());
        assert_eq!(
            ExportToken::decode(&s).unwrap_err().code,
            ErrorCode::InvalidCursor
        );
    }

    #[test]
    fn binding_checks() {
        let t = token(TokenKind::Checkpoint);
        assert_eq!(
            t.check_binding(TokenKind::Cursor, "inst-1", "10", 1, 100, 1100)
                .unwrap_err()
                .code,
            ErrorCode::InvalidCursor
        );
        assert_eq!(
            t.check_binding(TokenKind::Checkpoint, "other", "10", 1, 100, 1100)
                .unwrap_err()
                .code,
            ErrorCode::InstallationMismatch
        );
        assert_eq!(
            t.check_binding(TokenKind::Checkpoint, "inst-1", "11", 1, 100, 1100)
                .unwrap_err()
                .code,
            ErrorCode::EpochMismatch
        );
        assert_eq!(
            t.check_binding(TokenKind::Checkpoint, "inst-1", "10", 43, 100, 1100)
                .unwrap_err()
                .code,
            ErrorCode::CheckpointExpired
        );
        let c = ExportToken {
            kind: TokenKind::Cursor,
            ..token(TokenKind::Cursor)
        };
        assert_eq!(
            c.check_binding(
                TokenKind::Cursor,
                "inst-1",
                "10",
                1,
                100,
                2000 + SNAPSHOT_TTL_SECONDS
            )
            .unwrap_err()
            .code,
            ErrorCode::SnapshotExpired
        );
        c.check_binding(TokenKind::Cursor, "inst-1", "10", 1, 100, 2000)
            .unwrap();
    }

    #[test]
    fn watermark_beyond_current_history_expires() {
        // A restore or rewind leaves the store below the watermark a
        // token pinned: the sequence can never complete, so the token
        // expires instead of serving a stale page or empty delta.
        let t = token(TokenKind::Checkpoint);
        assert_eq!(
            t.check_binding(TokenKind::Checkpoint, "inst-1", "10", 1, 41, 1100)
                .unwrap_err()
                .code,
            ErrorCode::CheckpointExpired
        );
        let c = ExportToken {
            kind: TokenKind::Cursor,
            ..token(TokenKind::Cursor)
        };
        assert_eq!(
            c.check_binding(TokenKind::Cursor, "inst-1", "10", 1, 41, 1100)
                .unwrap_err()
                .code,
            ErrorCode::SnapshotExpired
        );
        // A checkpoint whose pinned lower boundary was purged can no
        // longer continue the consumer's committed revision sequence.
        assert_eq!(
            t.check_binding(TokenKind::Checkpoint, "inst-1", "10", 5, 100, 1100)
                .unwrap_err()
                .code,
            ErrorCode::CheckpointExpired
        );
        // A cursor whose lower checkpoint sits above its own pinned
        // watermark would emit after_seq > snapshot_seq.
        let bad = ExportToken {
            after_seq: Some(50),
            ..token(TokenKind::Cursor)
        };
        assert_eq!(
            bad.check_binding(TokenKind::Cursor, "inst-1", "10", 1, 100, 1100)
                .unwrap_err()
                .code,
            ErrorCode::InvalidCursor
        );
    }

    #[test]
    fn snapshot_ids_are_deterministic_and_distinct() {
        let a = snapshot_id("i", "e", None, 10, "n1");
        let b = snapshot_id("i", "e", None, 10, "n1");
        assert_eq!(a, b);
        // A fresh nonce is a fresh export sequence: same binding,
        // different id — the contract's polling guarantee.
        assert_ne!(a, snapshot_id("i", "e", None, 10, "n2"));
        assert_ne!(a, snapshot_id("i", "e", Some(5), 10, "n1"));
        assert_ne!(a, snapshot_id("i", "e", None, 11, "n1"));
    }
}
