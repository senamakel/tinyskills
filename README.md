# tinyskills

`tinyskills` is a host-independent Rust library for agentskills.io-style skill
bundles. It parses `SKILL.md` and `WORKFLOW.md`, normalizes metadata, discovers
bundles deterministically, resolves scope collisions, inventories and safely
reads resources, and materializes skills embedded in a host binary.

The crate deliberately does not decide where a product stores skills, whether
a workspace is trusted, how skills are executed, or how changes are announced.
Embedding applications provide their roots and retain those policy decisions.

## Example

```rust
use tinyskills::{DiscoveryRoot, SkillScope, discover};

let skills = discover([
    DiscoveryRoot::new("/home/me/.agents/skills", SkillScope::User),
    DiscoveryRoot::new("./.agents/skills", SkillScope::Project),
]);

for skill in skills {
    println!("{}: {}", skill.name, skill.description);
}
```

## Capabilities

- YAML frontmatter parsing with scalar or sequence `allowed-tools`
- current `WORKFLOW.md` / `SKILL.md` and legacy `skill.json` discovery
- recursive, deterministic scans that reject symlinked directories/manifests
- explicit scope precedence for builtin, legacy, user, project, and profile roots
- traversal-, symlink-, size-, and UTF-8-safe resource reads
- validated compile-time bundle content for store publication
- network-free registry catalog logic: Hermes catalog parsing, `SKILL.md` download-URL derivation (GitHub, ClawHub, skills.sh), entry lookup, and search filtering
- opt-in collision policies (`CollisionPolicy`, `TieBreak::LastWins`, excluded scopes) so root order can decide equal-scope ties
- store-backed authoring: `scaffold_bundle_in_store` stages in a caller-owned temporary directory, reads the store as the edit source of truth, and publishes with an expected revision; legacy roots are read-only import sources
- store-backed fetched and compiled installs: `install_document_in_store` and `install_compiled_in_store` publish validated bundle content without a local persistent-root fallback
- revision-checked deletion: `remove_bundle_from_store`
- filesystem staging helper: `scaffold_bundle_in_scratch` only changes caller-provided scratch space; it does not provide persistent storage
- `TriggerPattern` parsing and matching for `triggers:` frontmatter
- `read_document` for size-bounded, symlink-free, UTF-8 reads of documents and sidecars
- supply-chain scan of untrusted skill text: `scan_skill` over a `ScanDocument` and its bundled files returns `pass`/`warn`/`block` findings (invisible/bidi/zero-width code points, hard-coded credentials, and escaping resource paths block; agent-addressed text, fetch-and-exec pipelines, and undeclared MCP references warn), plus `sanitize_catalogue_text` for rendering untrusted text into a prompt as data
- product slug bounds: `SlugRules` (length cap, reserved names, truncation, a `PunctuationRule` that drops or folds punctuation, and a fallback slug for a name with no alphanumerics) for `slugify_with` and `validate_slug`
- a flat, line-based `SKILL.md` parser and renderer: `parse_flat`, `render_flat`, `split_frontmatter`, and `FlatSkill::scan_document` for the scan
- `document_digest`: the sha256 of one rendered document, for pinning an installed copy (not the same value as `BundledSkill::digest`)
- authoring budgets: `validate_description_chars` (counted in characters) and `check_frontmatter_size` (bytes in the frontmatter block)
- `materialize_tree`: rebuild a `<root>/<dir>/` tree inside an open parent directory handle from inline documents and open bundle directory handles, skipping symlinks, with checked names, bounded depth, and a per-file size cap. It takes `cap_std` handles (re-exported) and resolves no path, so opening the handles is the caller's job; the new tree is built beside the old one and swapped in on success
- `SkillRegistry` (feature `registry`): cached, searchable skill catalogs (the Hermes index, host-supplied static libraries) with stale-while-revalidate reads, single-flight refresh, ranked paged search, facets, and guarded `SKILL.md` fetches. Network I/O is host-supplied through `RegistryTransport`; the crate links no HTTP client
- `RunLogStore`: asynchronous start, append, tail, list, and clear operations over opaque cursors; `FileRunLogStore` keeps the existing `<workspace>/skills/.runs/*.log` layout, while `tinyskills-drivers` adds one `StreamStore` stream per run
- `read_skill_archive` (feature `archive`): reads a `.zip`/`.skill`, `.tar`, or `.tar.gz` upload into its `SKILL.md`, root directory, and bundled files. It refuses traversal, absolute or backslash paths, symlinks and hard links, and nested archives, and checks entry-count and expanded-size caps before reading any content

## User bundle content and persistence contract

`UserBundle::new` validates an in-memory file tree before it can be published:
portable relative names, case-insensitive collisions, file/directory conflicts,
and file-count and byte budgets. It preserves original document and resource
bytes, including empty and binary files. Files and manifests have deterministic
ordering; the bundle digest includes the selected primary document and every
file name and byte. It is a content identifier, not an access grant.

```rust
use tinyskills::{BundleDocument, BundleFile, BundleLimits, UserBundle};

let bundle = UserBundle::new(
    BundleDocument::Skill,
    "---\nname: example\ndescription: Example skill\n---\nHello\n".to_owned(),
    vec![BundleFile {
        path: "references/empty.txt".to_owned(),
        bytes: Vec::new(),
    }],
    BundleLimits::default(),
)?;
assert_eq!(bundle.file("references/empty.txt"), Some(&[][..]));
# Ok::<(), tinyskills::BundleContentError>(())
```

`BundleStore` is a separate asynchronous persistence contract for authored
bundles. It defines conditional publication and deletion, reads, and paged
listing. Implementations bind authority at construction, retain typed backend
error sources, and check that authority even for missing items and empty pages.
A committed head is authoritative; later cleanup failures must not imply that
publication rolled back. `Arc` forwarding preserves this contract.

This crate provides store-backed authoring and installation entrypoints over
the `BundleStore` contract, but no document/blob backend implementation or
compiled module. Hosts bind the store to their chosen authority and backend.
`materialize_tree` remains the explicit filesystem seam for building a local
discovery cache. The fetched-catalog `CatalogStore` is a separate contract.

## Which parser

`parse_skill_str` and `parse_skill` read frontmatter as YAML. Discovery uses
them, and they accept anything agentskills.io allows: sequences, nested
`metadata`, `allowed-tools` lists.

`parse_flat` reads one `key: value` per line and keeps only `name`,
`description`, `category`, and `version`, holding every other line, trimmed, in
`extra_frontmatter` so a scan can see it. The first non-empty `category` or
`version` wins; an empty one is kept as an extra line. Blank frontmatter lines
are discarded. Use it when a host stores, digests, and re-serves the document
itself: the body is kept byte for byte, and `render_flat` writes `name`,
`description`, a `category` and `version` only when non-empty, and then the
extra lines in stored order. `parse_flat → render_flat →
parse_flat` is a fixed point on the parsed value, not on the text: the
rendered document is canonical, so blank lines, key case, key order, and
whitespace around a line are not reproduced. `render_flat` cannot be made to
emit a second claim on a field or close the block early. A host pinning
installs with `document_digest` should digest `render_flat` output.

## Registry (feature `registry`)

`SkillRegistry` answers catalog queries for a host UI or agent tool. It is off
by default; enable it with `features = ["registry"]`.

```rust
use std::sync::Arc;

use tinyskills::{
    CatalogStore, EntryKey, HermesIndexSource, RegistryEntry, RegistryError,
    RegistryTransport, SkillQuery, SkillRegistry, StaticSource,
};

async fn browse(
    transport: impl RegistryTransport + 'static,
    packaged_entries: Vec<RegistryEntry>,
    store: impl CatalogStore + 'static,
) -> Result<(), RegistryError> {
    let registry: Arc<SkillRegistry> = SkillRegistry::builder(transport)
        .source(HermesIndexSource::hermes())
        .baseline(StaticSource::new("library", "Packaged library", packaged_entries))
        .store(store)
        .featured(["apple-notes"])
        .build();

    let page = registry.search(&SkillQuery::text("notes")).await?;
    println!("{} matching skills", page.total);
    let document = registry.fetch_document(&EntryKey::new("apple-notes")).await?;
    if document.is_blocked() { /* host policy decides */ }
    Ok(())
}
```

- **Reads** are stale-while-revalidate. A catalog within its time-to-live
  (default one hour) answers as `Live`; a stale one answers as `Cached` and is
  refreshed once in the background when a tokio runtime is running
  (`ReadPolicy::AllowStale`, the default), or refreshed first under
  `ReadPolicy::RequireFresh`. Concurrent refreshes of a source share one fetch.
- **Failures** keep the catalog held (`Cached`, with `SourceStatus::last_error`)
  and pause refreshes for the larger of `RegistryTimeouts::cooldown` and the
  upstream's `Retry-After`. With nothing cached, baselines answer as
  `LocalFallback`; with no baseline the typed `RegistryError` is returned.
  `RegistryError::kind()` is a stable `snake_case` code.
- **Search** matches the fields `filter_catalog` matches and ranks featured ids,
  then exact name, name prefix, name substring and other fields, installable
  entries first. Pages are 1-based, 25 entries by default, at most 100.
- **Documents**: `fetch_document` resolves the entry's URL (`skills.sh` entries
  probe their repo's conventional directories concurrently, then list its tree
  once), fetches it within 15 s and 1 MiB, validates it, parses it with
  `parse_flat`, digests it and scans it. A `ClawHub` 409 is
  `UpstreamAmbiguous`; a `LobeHub` agent is `NoDirectDownload`. Nothing is
  written to disk and a block verdict is reported, not enforced.
  `fetch_skill_document` does the same for a URL a user pastes.
- The serializable request and response types (`SkillQuery`, `SkillPage`,
  `SkillDetail`, `SourceStatus`, ...) are a versioned contract
  (`REGISTRY_CONTRACT_VERSION`) a host forwards over its own RPC.

### The transport contract

The host implements `RegistryTransport::send` for one HTTP exchange. Every
request has already passed the guard: `https` only (plain `http` to loopback
only with `FetchPolicy::allow_loopback_http`), host resolved through the
`Resolver`, every address public, and redirects re-validated hop by hop (at
most five). The transport must:

1. connect only to an address in `TransportRequest::pinned`, using the URL's
   host for TLS and the `Host` header;
2. not follow redirects, and return `3xx` responses as they are;
3. set `TransportResponse::final_url` to the requested URL;
4. send every header in `TransportRequest::headers` and apply
   `connect_timeout` to connection setup;
5. return the body lazily through `BodyChunks`, so the registry can stop at a
   size limit.

A sketch of a `reqwest` adapter (host code, not part of this crate):

```rust,ignore
struct ReqwestTransport;

impl RegistryTransport for ReqwestTransport {
    fn send(&self, request: TransportRequest) -> BoxFuture<'_, Result<TransportResponse, TransportError>> {
        Box::pin(async move {
            let url = reqwest::Url::parse(&request.url).map_err(|e| TransportError::Io(e.to_string()))?;
            let host = url.host_str().unwrap_or_default().to_owned();
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .connect_timeout(request.connect_timeout)
                .resolve_to_addrs(&host, &request.pinned)
                .build()
                .map_err(|e| TransportError::Connect(e.to_string()))?;
            let method = match request.method {
                HttpMethod::Head => reqwest::Method::HEAD,
                _ => reqwest::Method::GET,
            };
            let mut builder = client.request(method, url);
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            let response = builder.send().await.map_err(|e| {
                if e.is_timeout() { TransportError::Timeout } else { TransportError::Connect(e.without_url().to_string()) }
            })?;
            let headers = response
                .headers()
                .iter()
                .filter_map(|(k, v)| Some((k.to_string(), v.to_str().ok()?.to_owned())))
                .collect();
            Ok(TransportResponse::new(response.status().as_u16(), request.url, headers, Box::new(ReqwestBody(response))))
        })
    }
}

struct ReqwestBody(reqwest::Response);

impl BodyChunks for ReqwestBody {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, TransportError>> {
        Box::pin(async move {
            self.0.chunk().await.map(|c| c.map(|b| b.to_vec())).map_err(|e| TransportError::Io(e.without_url().to_string()))
        })
    }
}
```

## Development

Run from the repository root:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --all-targets --all-features
cargo test --all-features
cargo test --features registry
```

The registry tests run offline. `tests/fixtures/hermes/skills-sample.json`
holds 300 real entries from the Hermes skills index, captured on 2026-10-07,
covering every upstream source it aggregates (`ClawHub`, `skills.sh`,
`LobeHub`, GitHub, NVIDIA, OpenAI, Anthropic, HuggingFace, gstack, browse.sh,
and Hermes' built-in and optional skills).

## License

GPL-3.0-only. See [LICENSE](LICENSE).

## Storage ports

`tinyskills-drivers` (an unpublished workspace crate that depends on
`tinystoragedrivers-core` only) puts the host's storage under the crate:
`DocumentCatalogStore` keeps fetched registry catalogs as documents
(`skills.catalogs/<registry>`), `BlobBundles` keeps compiled bundle files as a
blob tree (`skills/bundles/<bundle id>/<path>`), and `DocumentBundleStore`
keeps authored bundle heads as CAS documents in `skills.bundles`. Its immutable
generations use the disjoint `skills/bundle-generations/<bundle id>/<generation>/<path>`
prefix, so legacy `BlobBundles` replacement and deletion cannot remove bytes
that a committed head references. `storage_schema()` declares these collections
and blob prefixes under the owner `skills` (tenant scope) for the host's `Registry::compose`,
with the step that moves the v1 `tinyskills_catalogs` collection into
`skills.catalogs`. v1 bundle blobs lived under a host-chosen prefix, so the
host moves them itself (a step cannot read a prefix that contains the new one). Discovery needs real directories, so
the host calls `BlobBundles::materialize` to rebuild a cache directory from the
blob tree; the write goes through `materialize_tree` with the new
`MaterializeSource::Files`. The host picks the backend and the tenant scope.
The crate ships no file-backed catalog store; `MemoryCatalogStore` covers tests and hosts without storage.
