//! Edge pins: the target-stability gate, and the only check in this build that
//! reads where a reference *landed*.
//!
//! [`crate::gate`] compares four integers — `resolved`, `external`,
//! `local_binding`, `unresolved`. A reference that resolves to the **wrong**
//! definition moves none of them: it is still one `Resolved` row and still one
//! edge, and only the far end changed. The rate cannot see it, the
//! `denominator_shrank` check cannot see it, and the two drift checks cannot
//! see it, because not one of them reads a target. The project's standing
//! verdict is that *a wrong edge is worse than a miss, because a miss is
//! counted and a wrong edge is not*; this module is the mechanism that counts
//! it.
//!
//! # The rule
//!
//! A pin file records, per corpus, the target of every resolved reference row.
//! A later scan of the same corpus may **add** rows — that is coverage growth
//! and it is legal. It may **never change the target of a row that already
//! resolved**: that is [`TargetMoved`], it fails by name, and it prints the
//! file, the line, the site text, the old FQN and the new one. A row that
//! **vanished** is flagged rather than failed — the counting gate owns that
//! half (`denominator_shrank` refuses a shrinking denominator, and the two
//! drift checks refuse a reference that walked into `external` or
//! `local_binding`), and a re-pin that drops rows shows the drop as deleted
//! lines in the pin file's own diff.
//!
//! # Why this format
//!
//! Written out in full — file, kind, declaration space, enclosing FQN, site
//! text, arity, target FQN — the eleven tier-1 corpora are 76,792 resolved
//! rows and **14.5 MB** of committed text (measured, not estimated). What the
//! check actually needs is far less: an identity per row that a later scan can
//! recompute, and the target it is pinned to. So a row is stored as **a
//! 64-bit hash of its canonical key bytes plus an index into a per-corpus
//! dictionary of target names**, and the eleven files come to **3.0 MB** — a
//! fifth of the size, with the target names still spelled out.
//!
//! Hashing the *key* is safe because a key that changes is a row that
//! appeared and a row that vanished — both are outcomes this check reports
//! without needing to read the key back. Hashing the *target* would not be:
//! a failure that printed `0x8f3a… became 0x1c07…` is useless to the person
//! who has to decide whether the new edge is right. The target dictionary is
//! therefore plaintext and deduplicated, so `was` is recoverable exactly, and
//! `now` — with the file, the line and the site text — is re-derived by
//! joining the failing hash against the scan that is already in hand. That is
//! the whole design constraint: **the failure path names rows, never
//! hashes.**
//!
//! Rows are grouped under their file, spelled out, so a diff of a re-pin says
//! *which files' edges moved* rather than scattering changed lines at random
//! through a two-megabyte sorted list.
//!
//! # The one command
//!
//! Every pin file carries, in its own header, the single command that
//! regenerates it:
//!
//! ```text
//! arthron pin corpus/go/codeiq --pins pins/go-codeiq.pins --write
//! ```
//!
//! Nothing else writes one, and nothing hand-edits one: a pin file edited by
//! hand is a claim about a scan that never ran.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::model::{DeclSpace, NodeId, RefKind};
use crate::store::{NodeRecord, ReadStore, RefKey, StoredOutcome};

/// The pin file format version this build reads and writes.
pub const FORMAT: u32 = 1;

/// One resolved reference row as a scan sees it: the key, where it starts, and
/// the name it resolved to.
///
/// The target is a *name*, not a [`NodeId`]: an identity is a 128-bit hash of
/// an FQN and cannot be turned back into one, and a pin that could not be read
/// back is a pin nobody can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRow {
    /// The row's key, exactly as the store holds it.
    pub key: RefKey,
    /// 1-based line of the row's first occurrence, for the failure message.
    /// Not part of the identity: a line moves whenever anything above it is
    /// edited, and an edge that did not move must not be reported as one.
    pub line: u32,
    /// The tagged name this row resolved to — `def <fqn>`, `pkg <path>` or
    /// `ext <package>`. Tagged because a definition's FQN and a package's
    /// import path live in one string space here, and an edge that moved from
    /// one to the other with the text unchanged would otherwise read as no
    /// movement at all.
    pub target: String,
}

/// A parsed pin file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pins {
    /// Format version. Always [`FORMAT`]; any other is refused rather than
    /// read under the wrong rules.
    pub format: u32,
    /// The corpus these pins were taken over. Provenance, and checked by the
    /// test that owns the file — not by this parser.
    pub corpus: String,
    /// The commit they were taken at. Provenance; printed, never verified.
    pub commit: String,
    /// Every distinct target name, sorted and deduplicated. A row names one by
    /// index.
    pub targets: Vec<String>,
    /// Pinned rows, by file: `(key hash, target index)`, sorted by hash.
    pub files: BTreeMap<String, Vec<(u64, u32)>>,
}

impl Pins {
    /// How many rows this file pins.
    pub fn rows(&self) -> u64 {
        self.files.values().map(|rows| rows.len() as u64).sum()
    }
}

/// A row's identity: blake3 over the canonical encoding of everything in its
/// key except the file, truncated to 64 bits.
///
/// The bytes hashed are exactly the ones [`RefKey::split`] produces for the
/// store, which that function documents as canonical — one key, one byte
/// string — so this identity is the store's own notion of row identity and
/// cannot drift from it. The file is not hashed because a pin file names it in
/// plaintext.
///
/// 64 bits, and not 128, because a collision here is not a wrong answer that
/// gets believed: [`render`] refuses to write a file in which two rows of one
/// file collide, and [`compare`] reports one as [`PinVerdict::collisions`]
/// rather than picking a side.
pub fn row_hash(key: &RefKey) -> u64 {
    let (_, rest) = key.split();
    let digest = blake3::hash(&rest);
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest.as_bytes()[..8]);
    u64::from_le_bytes(head)
}

/// Every resolved reference row in a store, sorted by `(file, key hash)`.
///
/// Reads [`ReadStore::for_each_row`] and not the candidate index, for the
/// reason that function gives: the rows are the store's own record of what
/// resolved, and a resolver that under-declared its candidates would make an
/// index-driven answer drop reference sites without saying so.
///
/// A row that resolved to an identity the node table does not hold is an
/// error and not a pin. That is a dangling edge — the graph claims a link to
/// something it cannot name — and pinning it under a placeholder would record
/// the defect as the expected answer.
pub fn collect(store: &ReadStore) -> Result<Vec<ResolvedRow>, String> {
    let mut resolved: Vec<(RefKey, u32, NodeId)> = Vec::new();
    let mut wanted: BTreeSet<NodeId> = BTreeSet::new();
    store.for_each_row(|key, record| {
        if let StoredOutcome::Resolved(id) = record.outcome {
            wanted.insert(id);
            resolved.push((key, record.first_line, id));
        }
        Ok(())
    })?;

    // Only the identities something resolved to, so a corpus with far more
    // definitions than resolved edges does not pay for the ones nothing
    // reaches.
    let mut names: BTreeMap<NodeId, String> = BTreeMap::new();
    store.for_each_node(|id, record| {
        if wanted.contains(&id) {
            names.insert(id, target_name(&record));
        }
        Ok(())
    })?;

    let mut rows = Vec::with_capacity(resolved.len());
    for (key, line, id) in resolved {
        let Some(target) = names.get(&id) else {
            return Err(format!(
                "{}:{line}: `{}` resolved to an identity the graph does not hold — \
                 a dangling edge, not a pin",
                key.file, key.raw_target,
            ));
        };
        rows.push(ResolvedRow {
            key,
            line,
            target: target.clone(),
        });
    }
    rows.sort_by(|a, b| {
        (a.key.file.as_str(), row_hash(&a.key)).cmp(&(b.key.file.as_str(), row_hash(&b.key)))
    });
    Ok(rows)
}

/// The tagged name a resolved identity carries.
fn target_name(record: &NodeRecord) -> String {
    match record {
        NodeRecord::Definition { fqn, .. } => format!("def {fqn}"),
        NodeRecord::Package { import_path, .. } => format!("pkg {import_path}"),
        NodeRecord::External { package, .. } => format!("ext {package}"),
    }
}

/// Render a pin file: the header that documents its own regeneration command,
/// then the target dictionary, then the rows.
///
/// Deterministic in the rows' order: the dictionary is sorted, the files are
/// sorted, and each file's rows are sorted by hash, so the same scan renders
/// the same bytes however the rows arrived.
///
/// # Errors
///
/// When two rows of one file share a key hash — which would make the file
/// ambiguous about which row a target belongs to — and when `corpus`, `commit`
/// or `pins_path` carries a character this format cannot represent.
pub fn render(
    corpus: &str,
    commit: &str,
    pins_path: &str,
    rows: &[ResolvedRow],
) -> Result<String, String> {
    for (field, value) in [("corpus", corpus), ("commit", commit), ("pins", pins_path)] {
        if !crate::gate::is_renderable(value) {
            return Err(format!(
                "`{field}` contains a quote, a backslash or a newline, which this \
                 pin format cannot represent: {value:?}",
            ));
        }
    }

    let mut targets: Vec<&str> = rows.iter().map(|r| r.target.as_str()).collect();
    targets.sort_unstable();
    targets.dedup();
    let index: BTreeMap<&str, u32> = targets
        .iter()
        .enumerate()
        .map(|(i, t)| (*t, u32::try_from(i).unwrap_or(u32::MAX)))
        .collect();
    if targets.len() > u32::MAX as usize {
        return Err("more distinct targets than a 32-bit index can name".to_string());
    }

    let mut files: BTreeMap<&str, Vec<(u64, u32)>> = BTreeMap::new();
    for row in rows {
        let hash = row_hash(&row.key);
        let target = index[row.target.as_str()];
        files
            .entry(row.key.file.as_str())
            .or_default()
            .push((hash, target));
    }
    for (file, entries) in &mut files {
        entries.sort_unstable();
        if let Some(pair) = entries.windows(2).find(|w| w[0].0 == w[1].0) {
            return Err(format!(
                "{file}: two rows share the key hash {:016x}; this file cannot say \
                 which target belongs to which, so it is not written",
                pair[0].0,
            ));
        }
    }

    let mut out = String::new();
    out.push_str(&header(corpus, commit, pins_path));
    let _ = writeln!(out, "format = {FORMAT}");
    let _ = writeln!(out, "corpus = \"{corpus}\"");
    let _ = writeln!(out, "commit = \"{commit}\"");
    let _ = writeln!(out, "targets = {}", targets.len());
    let _ = writeln!(out, "rows = {}", rows.len());
    out.push_str("\n[targets]\n");
    for target in &targets {
        let _ = writeln!(out, "{target}");
    }
    out.push_str("\n[rows]\n");
    for (file, entries) in &files {
        let _ = writeln!(out, "{file}");
        for (hash, target) in entries {
            let _ = writeln!(out, "\t{hash:016x} {target}");
        }
    }
    Ok(out)
}

/// The comment block every pin file opens with.
fn header(corpus: &str, commit: &str, pins_path: &str) -> String {
    format!(
        "\
# arthron edge pins — the target-stability gate.
#
# Regenerate this file with ONE command, from the repository root:
#
#   arthron pin {corpus} --pins {pins_path} --write --commit {commit}
#
# Nothing else writes this file and nothing hand-edits it. A pin file edited
# by hand is a claim about a scan that never ran.
#
# What it is. One line per resolved reference row, grouped under the file the
# reference sits in: the row's 64-bit key hash, and an index into the [targets]
# dictionary above it. The key hashed is the store's own canonical row key —
# kind, declaration space, enclosing FQN, site text, arity, binding verdict —
# so a row's identity here is the row's identity there.
#
# What it is for. `arthron gate` compares four integers, and a reference that
# resolves to the *wrong* definition moves none of them: it is still one
# resolved row and still one edge, and only the far end changed. This file is
# the only thing in the build that reads the far end.
#
# How it fails. A pinned row whose target changed fails by name —
# `target_moved` — printing the file, the line, the site text, the old target
# and the new one. A row that appeared is coverage growth and passes. A row
# that vanished is flagged, not failed: the counting gate owns that half, and a
# re-pin that drops rows shows the drop as deleted lines in this file's diff.
#
# Re-pinning is a deliberate act. Every changed target is a claim that the old
# edge was wrong, and belongs in docs/decisions.md with the reason.
#
# `corpus` and `commit` are provenance: printed, never verified.
# `targets` and `rows` are not a second copy of the body — they are a checksum
# over it, and a file whose header disagrees with its own sections is refused
# rather than read.
"
    )
}

/// Parse a pin file.
///
/// Strict on purpose. Comments and blank lines are permitted only in the
/// header, because after `[rows]` a line either names a file or is a row, and
/// a parser that quietly skipped what it did not recognise would answer with a
/// silently smaller pin set — which is a green build and the absence of a
/// check.
///
/// # Errors
///
/// A version this build does not read, a missing field, a section out of
/// order, a row before any file names it, a target index no dictionary entry
/// carries, and a header whose counts disagree with the body.
pub fn parse(text: &str) -> Result<Pins, String> {
    let mut format = None;
    let mut corpus = None;
    let mut commit = None;
    let mut want_targets = None;
    let mut want_rows = None;
    let mut targets: Vec<String> = Vec::new();
    let mut files: BTreeMap<String, Vec<(u64, u32)>> = BTreeMap::new();

    #[derive(PartialEq)]
    enum Section {
        Header,
        Targets,
        Rows,
    }
    let mut section = Section::Header;
    let mut file: Option<String> = None;

    for (n, line) in text.lines().enumerate() {
        let n = n + 1;
        match section {
            Section::Header => {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                if trimmed == "[targets]" {
                    section = Section::Targets;
                    continue;
                }
                let (key, value) = trimmed
                    .split_once('=')
                    .ok_or_else(|| format!("line {n}: not a `key = value` line: {trimmed:?}"))?;
                let (key, value) = (key.trim(), value.trim());
                let unquoted = |v: &str| -> Result<String, String> {
                    v.strip_prefix('"')
                        .and_then(|v| v.strip_suffix('"'))
                        .map(str::to_string)
                        .ok_or_else(|| format!("line {n}: `{key}` is not a quoted string"))
                };
                let number = |v: &str| -> Result<u64, String> {
                    v.parse::<u64>()
                        .map_err(|e| format!("line {n}: `{key}`: {e}"))
                };
                match key {
                    "format" => format = Some(number(value)?),
                    "corpus" => corpus = Some(unquoted(value)?),
                    "commit" => commit = Some(unquoted(value)?),
                    "targets" => want_targets = Some(number(value)?),
                    "rows" => want_rows = Some(number(value)?),
                    _ => return Err(format!("line {n}: unknown field `{key}`")),
                }
            }
            Section::Targets => {
                if line == "[rows]" {
                    section = Section::Rows;
                    continue;
                }
                if line.is_empty() {
                    continue;
                }
                targets.push(line.to_string());
            }
            Section::Rows => {
                if line.is_empty() {
                    continue;
                }
                if let Some(row) = line.strip_prefix('\t') {
                    let here = file.as_deref().ok_or_else(|| {
                        format!("line {n}: a row before any file names it: {row:?}")
                    })?;
                    let (hash, target) = row
                        .split_once(' ')
                        .ok_or_else(|| format!("line {n}: not a `<hash> <target>` row"))?;
                    let hash = u64::from_str_radix(hash, 16)
                        .map_err(|e| format!("line {n}: key hash: {e}"))?;
                    let target: u32 = target
                        .parse()
                        .map_err(|e| format!("line {n}: target index: {e}"))?;
                    files
                        .get_mut(here)
                        .ok_or_else(|| format!("line {n}: no rows started for `{here}`"))?
                        .push((hash, target));
                } else {
                    if files.contains_key(line) {
                        return Err(format!("line {n}: `{line}` is named twice"));
                    }
                    files.insert(line.to_string(), Vec::new());
                    file = Some(line.to_string());
                }
            }
        }
    }

    if section != Section::Rows {
        return Err("the file ends before its `[rows]` section".to_string());
    }
    let format = format.ok_or("no `format` field")?;
    let format = u32::try_from(format).map_err(|_| format!("format {format} is not readable"))?;
    if format != FORMAT {
        return Err(format!(
            "format {format} is not the format this build reads ({FORMAT})",
        ));
    }
    let corpus = corpus.ok_or("no `corpus` field")?;
    let commit = commit.ok_or("no `commit` field")?;

    // The two counts are a checksum over the body, not a second copy of it: a
    // file truncated by a failed write, or trimmed by an editor, reads as a
    // set of rows that quietly vanished, and a vanished row does not fail.
    let want_targets = want_targets.ok_or("no `targets` field")?;
    if want_targets != targets.len() as u64 {
        return Err(format!(
            "the header says {want_targets} targets and the body carries {}",
            targets.len(),
        ));
    }
    let rows: u64 = files.values().map(|r| r.len() as u64).sum();
    let want_rows = want_rows.ok_or("no `rows` field")?;
    if want_rows != rows {
        return Err(format!(
            "the header says {want_rows} rows and the body carries {rows}",
        ));
    }
    for (file, entries) in &files {
        for (hash, target) in entries {
            if *target as usize >= targets.len() {
                return Err(format!(
                    "{file}: row {hash:016x} names target {target}, and the dictionary \
                     holds {}",
                    targets.len(),
                ));
            }
        }
    }

    Ok(Pins {
        format,
        corpus,
        commit,
        targets,
        files,
    })
}

/// A pinned row whose target changed: the failure this whole module exists for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetMoved {
    /// Repo-relative file the reference sits in.
    pub file: String,
    /// 1-based line of its first occurrence in this scan.
    pub line: u32,
    /// The reference's kind, by name — `call`, `type-use`, `field-access`.
    pub kind: String,
    /// The declaration space it was looked up in.
    pub space: String,
    /// The edge's source: the nearest nameable encloser's FQN.
    pub enclosing: String,
    /// The literal text at the site.
    pub raw_target: String,
    /// Argument count at the site, when the extractor recorded one.
    pub argc: Option<u32>,
    /// The target it was pinned to.
    pub was: String,
    /// The target it resolves to now.
    pub now: String,
}

/// A pinned row this scan no longer produces.
///
/// Flagged, not failed — and it carries what it still can. There is no current
/// row to join against, so the site text and the line are gone with it; the
/// file, the old target and the key hash are what remain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowVanished {
    /// Repo-relative file the reference sat in.
    pub file: String,
    /// The row's key hash, as the pin file spells it.
    pub hash: u64,
    /// The target it was pinned to.
    pub was: String,
}

/// What a scan's resolved edges say about a corpus's pins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PinVerdict {
    /// Pinned rows whose target changed. Any one of these fails the gate.
    pub moved: Vec<TargetMoved>,
    /// Pinned rows this scan no longer produces. Flagged, not failed.
    pub vanished: Vec<RowVanished>,
    /// Rows this scan produced that no pin carries. Coverage growth; legal.
    pub appeared: u64,
    /// Pinned rows whose target is exactly where it was.
    pub held: u64,
    /// Files in which two of this scan's rows share a key hash, which makes
    /// the comparison ambiguous. Reported rather than resolved by picking a
    /// side, and fails the gate.
    pub collisions: Vec<String>,
}

impl PinVerdict {
    /// Whether this verdict fails a build.
    ///
    /// A moved target, or an ambiguity that stops one from being read. Not a
    /// vanished row: see the module header for which check owns that.
    pub fn failed(&self) -> bool {
        !self.moved.is_empty() || !self.collisions.is_empty()
    }

    /// The verdict as text a person can act on without opening the pin file.
    ///
    /// Every moved row is named in full. A failure that printed only a digest
    /// difference would tell a reader that something moved and nothing about
    /// what, which is the failure mode this format was chosen to avoid.
    pub fn report(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "pins: {} held, {} appeared, {} vanished, {} moved",
            self.held,
            self.appeared,
            self.vanished.len(),
            self.moved.len(),
        );
        for m in &self.moved {
            let argc = m.argc.map_or_else(|| "-".to_string(), |n| n.to_string());
            let _ = writeln!(
                out,
                "  target_moved {}:{} [{} {} argc={}] in {}\n    site  {}\n    was   {}\n    now   {}",
                m.file, m.line, m.kind, m.space, argc, m.enclosing, m.raw_target, m.was, m.now,
            );
        }
        for v in &self.vanished {
            let _ = writeln!(
                out,
                "  row_vanished {} [{:016x}]\n    was   {}",
                v.file, v.hash, v.was,
            );
        }
        for file in &self.collisions {
            let _ = writeln!(
                out,
                "  key_collision {file} — two rows of this file share a key hash",
            );
        }
        out
    }
}

/// Compare a scan's resolved rows against a corpus's pins.
///
/// The join is `(file, key hash)`, which is what the pin file is keyed by, so
/// a row that kept its key is compared on its target and a row that changed
/// its key is one that vanished and one that appeared. Both readings are
/// reported; only a changed target fails.
pub fn compare(pins: &Pins, rows: &[ResolvedRow]) -> PinVerdict {
    let mut verdict = PinVerdict::default();

    let mut current: BTreeMap<(&str, u64), &ResolvedRow> = BTreeMap::new();
    let mut collided: BTreeSet<&str> = BTreeSet::new();
    for row in rows {
        let key = (row.key.file.as_str(), row_hash(&row.key));
        if current.insert(key, row).is_some() {
            collided.insert(row.key.file.as_str());
        }
    }
    verdict.collisions = collided.into_iter().map(str::to_string).collect();

    let mut matched: BTreeSet<(&str, u64)> = BTreeSet::new();
    for (file, entries) in &pins.files {
        for (hash, target) in entries {
            let was = pins.targets[*target as usize].as_str();
            match current.get(&(file.as_str(), *hash)) {
                Some(row) => {
                    matched.insert((file.as_str(), *hash));
                    if row.target == was {
                        verdict.held += 1;
                    } else {
                        verdict.moved.push(TargetMoved {
                            file: row.key.file.clone(),
                            line: row.line,
                            kind: RefKind::from_code(row.key.kind).map_or_else(
                                || format!("kind {}", row.key.kind),
                                |k| k.name().to_string(),
                            ),
                            space: DeclSpace::from_code(row.key.space).map_or_else(
                                || format!("space {}", row.key.space),
                                |s| format!("{s:?}").to_lowercase(),
                            ),
                            enclosing: row.key.enclosing.clone(),
                            raw_target: row.key.raw_target.clone(),
                            argc: row.key.argc,
                            was: was.to_string(),
                            now: row.target.clone(),
                        });
                    }
                }
                None => verdict.vanished.push(RowVanished {
                    file: file.clone(),
                    hash: *hash,
                    was: was.to_string(),
                }),
            }
        }
    }
    verdict.appeared = current.keys().filter(|key| !matched.contains(*key)).count() as u64;
    verdict
}
