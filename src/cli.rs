//! Command-line surface. The whole product is meant to be two commands: `init` and `dev`.

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "nuthatch",
    version,
    about = "Be your own indexer - one binary, one command."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Scaffold an indexer for a contract: resolve its ABI and write a project here.
    Init(InitArgs),
    /// Add another contract to an existing nest - resolve its ABI and grow the config, no re-init.
    Add(AddArgs),
    /// Run the indexer: poll logs, store entities, and serve the API.
    Dev(DevArgs),
    /// Serve a nest **read-only** from a shared hot store, without indexing it (RFC-0022 slice 3).
    ///
    /// The query-FE half of scaled mode: it answers entity reads and SQL from state a *writer*
    /// elsewhere is filling, owns no cursor, and never advances one. Scale it independently of
    /// ingestion - that is the entire point of splitting the planes.
    Serve(ServeArgs),
    /// Run a **writer worker** for scaled mode (RFC-0022 §2): reconcile against the control plane,
    /// take cursor leases, index what this worker is assigned.
    ///
    /// This is the scaled-mode counterpart of `dev`. `dev` indexes a nest directory it owns outright;
    /// a worker is told what to run and holds a lease proving it may.
    Worker(WorkerArgs),
    /// Run the control-plane API for scaled mode (RFC-0022 §3): declare what the fleet should run.
    ///
    /// Writes *desired state* only. Workers reconcile against it on their own tick - nothing here
    /// commands a worker or waits for one.
    Control(ControlArgs),
    /// Query a nest's data with SQL - the live tip and sealed history, one surface. Prints a table.
    Sql(SqlArgs),
    /// Run a WASM transform component over a project's stored transfers.
    Transform(TransformArgs),
    /// Serve the Model Context Protocol over stdio (bridges to a running `nuthatch dev`).
    Mcp(McpArgs),
    /// Run a nest's invariant/parity checks (`checks/*.sql`) against recorded expected results.
    Check(CheckArgs),
    /// Regenerate the derived artifacts (`schema.json`, `llms.txt`, `semantic.toml` footguns) from
    /// `nuthatch.toml` - run after hand-editing the config (e.g. adding a factory `[[template]]`), so
    /// the schema and the derived `*_dec` columns match the tables the config now produces.
    Schema(SchemaArgs),
    /// Benchmark the indexing pipeline (measure first, optimise second - RFC-0004).
    Bench(BenchArgs),
    /// Probe an RPC endpoint before trusting a backfill to it: max `eth_getLogs` width, max JSON-RPC
    /// batch size, and archive depth - and print the largest safe `--window`. Each of those limits
    /// otherwise surfaces mid-backfill as a retry loop that looks like slowness.
    Doctor(DoctorArgs),
    /// Manage labeled address sets - the compliance annotation substrate (RFC-0008 C1).
    Labels(LabelsArgs),
    /// Manage sanctions/watch lists as content-addressed snapshots (RFC-0008 C2).
    Lists(ListsArgs),
    /// Screen sealed transfers against a list snapshot, recording `sanction_hit` annotations
    /// (RFC-0008 C2). Replayable: same list hash + range + component → identical hits.
    Screen(ScreenArgs),
    /// Build, sign, and verify the signed compliance-pack manifest (RFC-0008 C6).
    Pack(PackArgs),
    /// Audit the compliance annotations: `replay` re-proves them, `report` summarises them (C6).
    Audit(AuditArgs),
    /// Package a nest as a content-addressed blob - the deploy unit (RFC-0012).
    Nest(NestArgs),
    /// Derive-first recipes (RFC-0023): add a view that computes a read (e.g. `total_supply`) from
    /// indexed events instead of fetching it with an `eth_call`. No archive node, deterministic, free.
    Recipe(RecipeArgs),
    /// Fetch + cache a token's immutable metadata - `decimals`/`symbol`/`name` (RFC-0023 tier 2). Called
    /// once (they never change) and remembered in `metadata.json`; the constants tier 1 can't derive.
    Metadata(MetadataArgs),
    /// Move a pre-2.0 directory to identity-keyed datasets: `nests/<name>/` becomes `data/<nid>/`,
    /// and `roost.toml` becomes `mounts.toml` (RFC-0032).
    ///
    /// Data is moved, never re-indexed. Idempotent, and safe to run on a directory that is already
    /// partly migrated. Use `--dry-run` first: it prints the entire plan and changes nothing.
    Migrate(MigrateArgs),
    /// Reclaim the disk of datasets nothing mounts any more (RFC-0032 §5).
    ///
    /// Unmounting a nest never deletes its data - re-backfilling is the cost identity-keyed storage
    /// exists to avoid, so unmount/remount is free. This is the explicit way to get the disk back.
    /// It LISTS by default; `--yes` is what deletes.
    Prune(PruneArgs),
    /// Regenerate the builder skill's machine-generated references from clap metadata (RFC-0017).
    /// Hidden: a dev/authoring tool, not part of the user-facing two-command story.
    #[command(hide = true)]
    SkillRefs,
}

#[derive(Args)]
pub struct MigrateArgs {
    /// The directory to migrate: a pre-2.0 one with a roost.toml, or one already part-migrated.
    #[arg(long, default_value = ".")]
    pub dir: String,
    /// Print the plan - every nest, its identity, and where it would land - without changing
    /// anything.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args)]
pub struct PruneArgs {
    /// The directory to migrate: a pre-2.0 one with a roost.toml, or one already part-migrated.
    #[arg(long, default_value = ".")]
    pub dir: String,
    /// Actually delete. Without this, prune only reports what it would remove.
    #[arg(long)]
    pub yes: bool,
}

#[derive(Args)]
pub struct NestArgs {
    #[command(subcommand)]
    pub what: NestWhat,
}

#[derive(Args)]
pub struct RecipeArgs {
    #[command(subcommand)]
    pub what: RecipeWhat,
}

#[derive(Args)]
pub struct MetadataArgs {
    #[command(subcommand)]
    pub what: MetadataWhat,
}

#[derive(Subcommand)]
pub enum MetadataWhat {
    /// Fetch + cache immutable metadata for every contract in the nest (skips already-cached ones).
    Fetch(MetadataFetchArgs),
}

#[derive(Args)]
pub struct MetadataFetchArgs {
    /// The nest directory.
    #[arg(long, default_value = ".")]
    pub dir: String,
    /// Override `rpc_urls` at runtime (repeatable); tried ahead of the configured endpoints.
    #[arg(long)]
    pub rpc: Vec<String>,
}

#[derive(Subcommand)]
pub enum RecipeWhat {
    /// List the available derive-first recipes.
    List,
    /// Add a recipe's derived view to a nest's `views/` (never clobbers an existing file).
    Add(RecipeAddArgs),
}

#[derive(Args)]
pub struct RecipeAddArgs {
    /// The recipe name (see `nuthatch recipe list`), e.g. `total_supply`.
    pub name: String,
    /// The contract alias whose table the view derives over. Defaults to the nest's first contract.
    #[arg(long)]
    pub alias: Option<String>,
    /// The nest directory.
    #[arg(long, default_value = ".")]
    pub dir: String,
}

#[derive(Subcommand)]
pub enum NestWhat {
    /// Bundle a nest into one portable, content-addressed `.bundle` file - its authored inputs (config,
    /// ABIs, views, labels, skills) plus a `manifest.json` pinning the expected decode-registry hash.
    /// Share the `.bundle` anywhere (a URL, a file); anyone can `load` it to run your exact nest,
    /// verified by hash. Prints the bundle's content address.
    Bundle(NestBundleArgs),
    /// Load a bundle: verify a `.bundle` (or a URL to one, or an unpacked bundle dir) and install it as
    /// a runnable nest. Checks the manifest format, every file's hash, and that the decode registry
    /// regenerated from the inputs matches the manifest - so a loaded nest decodes exactly as authored.
    /// With `--registry`, the positional is a `name[@version]` reference resolved against that store.
    Load(NestLoadArgs),
    /// Publish a `.bundle` to a registry (RFC-0019) under `name@version`, advancing `latest`. The
    /// registry is a decoupled, optional store: a filesystem path, or S3-compatible object storage
    /// (`s3://bucket/prefix`, configured with the usual `AWS_*` env - needs a build with
    /// on by default). A self-built bundle and `nest load <file|dir>` never need one.
    /// Prints the content address.
    Publish(NestPublishArgs),
    /// Classify an update between two nests as compatible or breaking (RFC-0020). Compatible = additive
    /// only (safe to hot-swap on the same endpoint); breaking = a consumer-observable change (needs a
    /// new endpoint). Each argument is a nest directory or a `schema.json` path.
    Diff(NestDiffArgs),
    /// Hot-upgrade a running nest to a compatible new version with zero downtime (RFC-0020): serve the
    /// old version, index the new one concurrently, then atomically flip the endpoint once it catches
    /// up. A breaking update is refused (it needs a new endpoint). The served address never changes.
    Upgrade(NestUpgradeArgs),
}

#[derive(Args)]
pub struct NestBundleArgs {
    /// Nest directory to bundle.
    #[arg(default_value = ".")]
    pub dir: String,

    /// Output path for the `.bundle` (default: `<nest-name>-<hash>.bundle` beside the nest). With
    /// `--as-dir`, an unpacked bundle *directory* is written here instead of a single file.
    #[arg(long)]
    pub out: Option<String>,

    /// Write an unpacked bundle directory instead of a single `.bundle` file (handy for inspecting a
    /// bundle's contents).
    #[arg(long)]
    pub as_dir: bool,
}

#[derive(Args)]
pub struct NestLoadArgs {
    /// The bundle to load: a `.bundle` file, an `http(s)://` URL to one, or an unpacked bundle
    /// directory - or, with `--registry`, a `name[@version]` reference (no `@version` → `latest`).
    pub bundle: String,

    /// Target directory to install the nest into (default: the nest's name).
    #[arg(long)]
    pub dir: Option<String>,

    /// Assert the bundle's content-address hash equals this value before installing.
    #[arg(long)]
    pub expect: Option<String>,

    /// Resolve the positional as a `name[@version]` reference against this registry (RFC-0019). A
    /// filesystem path, or `s3://bucket/prefix` (S3/MinIO/R2, via the usual `AWS_*` env). The pulled blob is
    /// hash-verified on install.
    #[arg(long)]
    pub registry: Option<String>,
}

#[derive(Args)]
pub struct NestPublishArgs {
    /// The `.bundle` file to publish (from `nuthatch nest bundle`).
    pub bundle: String,

    /// The registry to publish to (RFC-0019). A filesystem path, or `s3://bucket/prefix` with
    /// configured via the usual `AWS_*` env (S3/MinIO/R2).
    #[arg(long)]
    pub registry: String,

    /// Publish as `name` or `name@version`. Defaults: name = the bundle's nest name; version =
    /// `h<hash12>` (a content-addressed label - semantic versions are RFC-0020's concern).
    #[arg(long = "as")]
    pub as_ref: Option<String>,
}

#[derive(Args)]
pub struct NestDiffArgs {
    /// The old (current) version: a nest directory or a `schema.json` path.
    pub old: String,
    /// The new (proposed) version: a nest directory or a `schema.json` path.
    pub new: String,
}

#[derive(Args)]
pub struct NestUpgradeArgs {
    /// The running/current nest directory - the OLD version being upgraded from.
    #[arg(long, default_value = ".")]
    pub dir: String,

    /// The NEW version to upgrade to: a prepared nest directory (e.g. from `nest load`).
    #[arg(long)]
    pub to: String,

    /// Address to serve on. For a compatible update the endpoint stays the same across the flip; for a
    /// breaking one the old version stays here (deprecated) and the new is served under `--new-endpoint`.
    #[arg(long, default_value = "127.0.0.1:8288")]
    pub listen: String,

    /// For a BREAKING update, the path prefix the new version is served under while the old stays at
    /// root (deprecated). Default `next` → served at `/next`. Ignored for a compatible update.
    #[arg(long, default_value = "next")]
    pub new_endpoint: String,

    /// Override `rpc_urls` at runtime (repeatable); tried ahead of the configured endpoints.
    #[arg(long)]
    pub rpc: Vec<String>,

    /// Backfill the new version's finalized history straight to Parquet before tip-following (RFC-0004).
    #[arg(long)]
    pub seal_direct: bool,

    /// Concurrent window fetches during the new version's seal-direct backfill.
    #[arg(long, default_value_t = 1)]
    pub concurrency: usize,

    /// Override the `eth_getLogs` block-window for the new version's backfill.
    #[arg(long)]
    pub window: Option<u64>,

    /// Disable the built-in admin UI entirely.
    #[arg(long)]
    pub no_admin: bool,
}

#[derive(Args)]
pub struct PackArgs {
    #[command(subcommand)]
    pub what: PackWhat,
}

#[derive(Subcommand)]
pub enum PackWhat {
    /// Generate an ed25519 signing keypair into a local JSON file.
    Keygen(PackKeygenArgs),
    /// Assemble `compliance-pack.toml` from the nest's config + artifact hashes (optionally signed).
    Build(PackBuildArgs),
    /// Verify a pack: signature, artifact hashes, and grant conformance.
    Verify(PackVerifyArgs),
}

#[derive(Args)]
pub struct PackKeygenArgs {
    /// Where to write the keypair JSON.
    #[arg(long, default_value = "nuthatch-key.json")]
    pub out: String,
}

#[derive(Args)]
pub struct PackBuildArgs {
    /// Nest directory.
    #[arg(long, default_value = ".")]
    pub dir: String,
    /// Sign the manifest with this keypair file (from `pack keygen`). Omit to write it unsigned.
    #[arg(long)]
    pub key: Option<String>,
}

#[derive(Args)]
pub struct PackVerifyArgs {
    /// Nest directory (must contain a `compliance-pack.toml`).
    #[arg(long, default_value = ".")]
    pub dir: String,
}

#[derive(Args)]
pub struct AuditArgs {
    #[command(subcommand)]
    pub what: AuditWhat,
}

#[derive(Subcommand)]
pub enum AuditWhat {
    /// Re-run screening over the sealed segments and confirm the stored hits reproduce exactly.
    Replay(AuditReplayArgs),
    /// Summarise the hits and flags in a block range (markdown or `--json`).
    Report(AuditReportArgs),
}

#[derive(Args)]
pub struct AuditReplayArgs {
    /// Nest directory.
    #[arg(long, default_value = ".")]
    pub dir: String,
    /// First block of the range (inclusive).
    #[arg(long)]
    pub from: u64,
    /// Last block of the range (inclusive).
    #[arg(long)]
    pub to: u64,
}

#[derive(Args)]
pub struct AuditReportArgs {
    /// Nest directory.
    #[arg(long, default_value = ".")]
    pub dir: String,
    /// First block of the range (inclusive).
    #[arg(long)]
    pub from: u64,
    /// Last block of the range (inclusive).
    #[arg(long)]
    pub to: u64,
    /// Emit JSON instead of markdown.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct ListsArgs {
    #[command(subcommand)]
    pub what: ListsWhat,
}

#[derive(Subcommand)]
pub enum ListsWhat {
    /// Fetch a sanctions/watch list (host-side, out-of-band) into a content-addressed snapshot.
    Fetch(ListsFetchArgs),
    /// List the fetched list snapshots and how many addresses each carries.
    List(ListsListArgs),
}

#[derive(Args)]
pub struct ListsFetchArgs {
    /// List name. Known: `ofac-sdn`, `eu-consolidated` (have default URLs). Any other name needs
    /// `--url` or `--file`.
    pub list: String,

    /// Read the list from a local file instead of downloading (any text: XML/CSV/JSON/plain - the
    /// fetcher extracts every `0x…40hex` address).
    #[arg(long)]
    pub file: Option<String>,

    /// Override the download URL for this list.
    #[arg(long)]
    pub url: Option<String>,

    /// Nest directory to fetch into (writes `lists/<hash>.json`).
    #[arg(long, default_value = ".")]
    pub dir: String,
}

#[derive(Args)]
pub struct ListsListArgs {
    /// Nest directory to read list snapshots from.
    #[arg(long, default_value = ".")]
    pub dir: String,
}

#[derive(Args)]
pub struct ScreenArgs {
    /// The list snapshot hash to screen against (from `nuthatch lists fetch`).
    #[arg(long)]
    pub list: String,

    /// First block of the range to screen (inclusive).
    #[arg(long)]
    pub from: u64,

    /// Last block of the range to screen (inclusive).
    #[arg(long)]
    pub to: u64,

    /// Nest directory (must contain a `nuthatch.toml` and sealed segments over the range).
    #[arg(long, default_value = ".")]
    pub dir: String,
}

#[derive(Args)]
pub struct LabelsArgs {
    #[command(subcommand)]
    pub what: LabelsWhat,
}

#[derive(Subcommand)]
pub enum LabelsWhat {
    /// Import a labeled address set (CSV `address,label` or JSON) as a content-addressed snapshot.
    Import(LabelsImportArgs),
    /// List the imported label snapshots and how many addresses each carries.
    List(LabelsListArgs),
}

#[derive(Args)]
pub struct LabelsImportArgs {
    /// Path to the label file: CSV (`address,label` per line, optional header) or JSON (an array of
    /// `{address,label}` objects, or an `{address: label}` map).
    pub file: String,

    /// Nest directory to import into (writes `labels/<hash>.json`).
    #[arg(long, default_value = ".")]
    pub dir: String,
}

#[derive(Args)]
pub struct LabelsListArgs {
    /// Nest directory to read labels from.
    #[arg(long, default_value = ".")]
    pub dir: String,
}

#[derive(Args)]
pub struct BenchArgs {
    #[command(subcommand)]
    pub what: BenchWhat,
}

#[derive(Subcommand)]
pub enum BenchWhat {
    /// Measure backfill throughput (events/sec, wall-clock, peak RSS) over a pinned block range.
    Backfill(BackfillBenchArgs),
    /// Measure the read path: entity point-read latency (p50/p99) and the `/sql` hot∪cold scan cost
    /// (query latency + peak RSS). The regression guard for the perf refactors - run offline against
    /// an already-indexed nest.
    Query(QueryBenchArgs),
}

#[derive(Args)]
pub struct QueryBenchArgs {
    /// Nest directory (must contain an indexed `nuthatch.redb`). Stop `dev` first - the bench opens
    /// the store directly.
    #[arg(long, default_value = ".")]
    pub dir: String,

    /// The `/sql` query to time (over hot∪cold). Defaults to `SELECT count(*)` on the largest hot
    /// table - the full-tip-materialising scan whose cost is the #1 RAM risk on deep-finality L2s.
    #[arg(long)]
    pub sql: Option<String>,

    /// Entity point-reads to time (keys sampled evenly across the hot store).
    #[arg(long, default_value_t = 1000)]
    pub reads: usize,

    /// `/sql` query repetitions to time (the report is the p50/p99 across them).
    #[arg(long, default_value_t = 20)]
    pub iters: usize,

    /// Write the bench-report JSON here. Prints to stdout regardless.
    #[arg(long)]
    pub out: Option<String>,

    /// A label for the report (e.g. "R1: horizon tip 12k rows").
    #[arg(long)]
    pub label: Option<String>,
}

#[derive(Args)]
pub struct BackfillBenchArgs {
    /// Nest directory (must contain a `nuthatch.toml`).
    #[arg(long, default_value = ".")]
    pub dir: String,

    /// First block of the pinned range (inclusive).
    #[arg(long)]
    pub from: u64,

    /// Last block of the pinned range (inclusive).
    #[arg(long)]
    pub to: u64,

    /// How many runs to take (the report is the median). Public RPC is noisy - 3 is sensible.
    #[arg(long, default_value_t = 3)]
    pub runs: usize,

    /// Override the nest's `rpc_urls` (e.g. point at your own archive node for a T2 run).
    #[arg(long)]
    pub rpc: Option<String>,

    /// Write the bench-report JSON here (e.g. `docs/bench/w1.json`). Prints to stdout regardless.
    #[arg(long)]
    pub out: Option<String>,

    /// A label for the workload in the report (e.g. "W1: USDC 100k dense").
    #[arg(long)]
    pub label: Option<String>,

    /// Measure the seal-direct path (decode → Parquet, bypassing the hot store) instead of the
    /// default decode → redb hot-store path. Use to compare the two backfill storage paths.
    #[arg(long)]
    pub seal_direct: bool,

    /// Concurrent window fetches (seal-direct only). >1 overlaps RPC round-trip latency; results are
    /// still consumed in block order so segments are identical. Try 8-16 against your own node.
    #[arg(long, default_value_t = 1)]
    pub concurrency: usize,
}

#[derive(Args)]
pub struct CheckArgs {
    /// Optional check-name filter (substring). Omit to run every `checks/*.sql`. E.g. `parity`.
    pub name: Option<String>,

    /// Nest directory (must contain a `checks/` folder).
    #[arg(long, default_value = ".")]
    pub dir: String,

    /// Record current query results as the expected fixtures (`checks/expected/*.json`) instead of
    /// comparing - the authoring mode, run once against known-good sealed data.
    #[arg(long)]
    pub update: bool,
}

#[derive(Args)]
pub struct SchemaArgs {
    /// Nest directory (must contain a `nuthatch.toml`).
    #[arg(long, default_value = ".")]
    pub dir: String,
}

#[derive(Args)]
pub struct McpArgs {
    /// Base URL of the running `nuthatch dev` HTTP API to bridge to.
    #[arg(long, default_value = "http://127.0.0.1:8288")]
    pub url: String,

    /// Print a copy-paste MCP client config (Claude Code `.mcp.json` + the `claude mcp add` one-liner)
    /// and exit, instead of running the stdio server. This is the "wire it up in one step" helper.
    #[arg(long)]
    pub print_config: bool,
}

#[derive(Args)]
pub struct SqlArgs {
    /// The SQL query (SELECT/WITH). Tables are `{alias}__{event}`, e.g. `usdc__transfer`. Omit to open
    /// an interactive REPL (`.tables`, `.schema <t>`, history; `.exit` to quit).
    pub query: Option<String>,

    /// Nest directory (queried directly when no `nuthatch dev` holds the store).
    #[arg(long, default_value = ".")]
    pub dir: String,

    /// The running instance's API, used when the local store is locked by `nuthatch dev`.
    #[arg(long, default_value = "http://127.0.0.1:8288")]
    pub url: String,

    /// Emit newline-delimited JSON instead of a table (for piping to jq etc.).
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct TransformArgs {
    /// Path to the transform component (.wasm, wasm32-wasip2).
    pub component: String,

    /// Project directory (must contain a nuthatch.redb with indexed transfers).
    #[arg(long, default_value = ".")]
    pub dir: String,

    /// How many of the most-recent transfers to feed the transform.
    #[arg(long, default_value_t = 5_000)]
    pub limit: usize,
}

#[derive(Args)]
pub struct InitArgs {
    /// One or more contract addresses to index, e.g. 0xA0b8…eB48 (USDC). Omit when using `--from`.
    #[arg(num_args = 0..)]
    pub addresses: Vec<String>,

    /// Initialise from a published nest instead of addresses: a git URL or a local directory. The
    /// nest is self-contained (ABIs vendored), so nothing is resolved - just cloned/copied + validated.
    #[arg(long, conflicts_with = "addresses")]
    pub from: Option<String>,

    /// Scaffold from a Graph Protocol subgraph manifest: an IPFS CID (`QmVPhL…`, `ipfs://QmVPhL…`)
    /// or a URL to a `subgraph.yaml`. Maps `dataSources` → `[[contracts]]` and `templates` →
    /// `[[templates]]`, vendors every ABI from its pinned CID, and carries `startBlock` across.
    ///
    /// The ABIs a manifest pins are strictly better than Sourcify for proxy-heavy codebases:
    /// Sourcify returns the proxy ABI, which declares none of the events the contract emits.
    ///
    /// Gateway responses are trusted, not verified: nothing recomputes the hash over the bytes
    /// that come back, so a CID names what to ask for rather than proving what arrived.
    ///
    /// Which template a factory creates lives in the mapping WASM, not the manifest, so factory
    /// rules are inferred only where a parameter unambiguously names a template. Everything else is
    /// reported by name with candidates for you to resolve - never guessed.
    #[arg(long, conflicts_with_all = ["addresses", "from", "alias", "abi"], value_name = "CID_OR_URL")]
    pub from_subgraph: Option<String>,

    /// IPFS gateway(s) to try, in order, when resolving `--from-subgraph` (repeatable). Each is a
    /// URL prefix that a CID is appended to. Defaults to The Graph's gateway, then ipfs.io.
    #[arg(long, value_name = "URL_PREFIX")]
    pub ipfs: Vec<String>,

    /// Optional aliases, one per address in order (comma-separated). Defaults to c0, c1, ….
    #[arg(long, value_delimiter = ',')]
    pub alias: Vec<String>,

    /// Use these local ABI file(s) instead of resolving from Sourcify/Etherscan, one per address in
    /// order (comma-separated; use an empty entry to resolve that address normally). The escape
    /// hatch for a proxy whose implementation ABI the public resolvers don't return - point this at
    /// the implementation's ABI and the nest decodes the events the proxy actually emits.
    #[arg(long, value_delimiter = ',')]
    pub abi: Vec<String>,

    /// Chain to index, e.g. mainnet, arbitrum-one, base. Omit it and nuthatch probes each known
    /// chain for the contract's bytecode and picks the one it lives on - you rarely need to say.
    #[arg(long)]
    pub chain: Option<String>,

    /// Prefer these RPC URL(s) over the chain defaults (repeatable). They're written first in the
    /// nest's `rpc_urls` and also used for ABI/deploy-block resolution during init, with the
    /// built-in chain endpoints kept as fallback. Point at your own node to dodge public-RPC limits.
    #[arg(long)]
    pub rpc: Vec<String>,

    /// Directory to scaffold into (defaults to the current directory; for `--from`, defaults to the
    /// nest's own name).
    #[arg(long, default_value = ".")]
    pub dir: String,

    /// Don't index block timestamps: drop the implicit `block_timestamp` column from every table.
    ///
    /// Fetching timestamps is roughly 85% of backfill wall clock (RFC-0029 §4) because they arrive
    /// per-block over a separate round trip that `eth_getLogs` doesn't carry - so a nest that never
    /// needs them backfills dramatically faster.
    ///
    /// **Declare it here or not at all.** This is not a flag you can flip later: it removes a column
    /// from every table (breaking, for anyone querying it) and changes the bytes of every sealed
    /// segment, so a nest that switches cannot reuse its own segments and must re-index from scratch.
    /// A nest that has already indexed refuses to start if this disagrees with what it was built
    /// with; changing your mind means a new nest served alongside the old one (RFC-0020 slice 3).
    ///
    /// Say no to this if you are unsure - `block_timestamp` is the column most views end up wanting,
    /// and time-series questions ("per day", "per hour") cannot be asked without it.
    #[arg(long)]
    pub no_timestamps: bool,
}

#[derive(Args)]
pub struct AddArgs {
    /// One or more contract addresses to add to the nest, e.g. 0xC02a…6Cc2 (WETH).
    #[arg(num_args = 1..)]
    pub addresses: Vec<String>,

    /// Optional aliases, one per address in order (comma-separated). Defaults to the next free
    /// c<N> slots after the nest's existing contracts.
    #[arg(long, value_delimiter = ',')]
    pub alias: Vec<String>,

    /// Use these local ABI file(s) instead of resolving from Sourcify/Etherscan, one per address in
    /// order (comma-separated; an empty entry resolves that address normally). Same proxy escape
    /// hatch as `init --abi`.
    #[arg(long, value_delimiter = ',')]
    pub abi: Vec<String>,

    /// The nest directory to grow (must contain a nuthatch.toml). Defaults to the current directory.
    #[arg(long, default_value = ".")]
    pub dir: String,

    /// Prefer these RPC URL(s) over the nest's configured endpoints for ABI/deploy-block resolution
    /// (repeatable). Point at your own node to dodge public-RPC limits.
    #[arg(long)]
    pub rpc: Vec<String>,
}

#[derive(Args)]
pub struct WorkerArgs {
    /// The control-plane Postgres URL - desired state and the worker registry.
    #[arg(long)]
    pub control_db: String,

    /// The hot-store Postgres URL - where indexed state lives. Usually the same server as the control
    /// plane, but a distinct database: one holds *what should run*, the other holds *indexed data*.
    #[arg(long)]
    pub hot_store: String,

    /// Chains this worker can host. The scheduler decides which it *should* run and the lease decides
    /// which it *does*; this is only what the machine is capable of.
    #[arg(long, value_delimiter = ',', required = true)]
    pub chains: Vec<String>,

    /// This worker's identity in the fleet. Defaults to the hostname, which is unique per container
    /// under compose. **Two workers sharing an id would look like one to the registry** and could each
    /// hold what it believed was its own lease, so this is derived rather than left to chance.
    #[arg(long)]
    pub id: Option<String>,

    /// RAM ceiling for everything this worker runs, in MB. The scheduler will not commit cursors past
    /// it - Σ assigned cursors ≤ this.
    #[arg(long, default_value_t = 2048)]
    pub budget_mb: u64,

    /// Where to find the nests this worker is asked to index. `<root>/<name>` if that exists, else
    /// `<root>` itself when it is a single nest - which is what the compose topology mounts.
    ///
    /// What is here wins over the registry: a nest an operator placed on the machine is a deliberate
    /// act, and this mount is read-only in the writer-node topology. Anything not here is pulled from
    /// `--registry` into `--nest-cache`.
    #[arg(long, default_value = "/nest")]
    pub nest_root: String,

    /// Registry to pull declared nests from when they are not under `--nest-root` (RFC-0019).
    ///
    /// A locator the bundle store understands - a local directory, or `s3://…` for a shared registry.
    /// When the control plane has pinned a `bundle_hash`, the fetch goes **by that hash** and the
    /// registry's mutable index is never consulted: re-tagging a version cannot change what a worker
    /// runs. Unpinned, a worker is exactly as trustworthy as its registry - so pin.
    ///
    /// Without this, a worker can only run nests an operator has already placed on the machine.
    #[arg(long)]
    pub registry: Option<String>,

    /// Where pulled nests are cached. Written to; keep it off the read-only `--nest-root` mount.
    ///
    /// Laid out `<cache>/<name>/<bundle-hash>`, so a re-pinned fleet resolves to a different
    /// directory and re-pulls, while an unchanged pin costs no download. Safe to delete: everything
    /// in it is re-fetchable, and a worker rebuilds what it needs on the next tick.
    #[arg(long, default_value = "/var/lib/nuthatch/nests")]
    pub nest_cache: String,

    /// Skip fetching runtime secrets for assigned nests (RFC-0022 §5).
    #[arg(long)]
    pub no_secrets: bool,
}

#[derive(Args)]
pub struct ControlArgs {
    /// Address to bind the control-plane API to. Off-localhost requires NUTHATCH_CONTROL_TOKEN and
    /// refuses to start without it.
    #[arg(long, default_value = "127.0.0.1:8290")]
    pub listen: String,

    /// The control-plane Postgres URL, e.g. `postgres://user:pass@host/db`. This is desired state,
    /// **not** a nest's hot store - keep them separate so a fleet-wide outage and a single cursor's
    /// outage are different events.
    #[arg(long)]
    pub db: String,
}

#[derive(Args)]
pub struct ServeArgs {
    /// Project directory (must contain a nuthatch.toml).
    #[arg(long, default_value = ".")]
    pub dir: String,

    /// Address to bind the HTTP API to.
    #[arg(long, default_value = "127.0.0.1:8288")]
    pub listen: String,

    /// Postgres hot store to serve from, e.g. `postgres://user:pass@host/db`. Omit to serve the
    /// nest's local redb, which read-scales a single box but shares a file rather than a service.
    /// Requires a build with `--features postgres-store`.
    #[arg(long)]
    pub hot_store: Option<String>,

    /// Serve the admin UI. Off by default and deliberately *not* symmetrical with `dev`: an FE node
    /// owns no cursor, so the lifecycle routes it would expose have nothing to act on.
    #[arg(long)]
    pub admin: bool,
}

#[derive(Args)]
pub struct DevArgs {
    /// The directory to run. A `nuthatch.toml` runs that one nest; a `mounts.toml` runs every nest
    /// it mounts, one isolated cursor per chain (RFC-0032). One command either way.
    #[arg(long, default_value = ".")]
    pub dir: String,

    /// Exit on the first fault instead of quarantining it (RFC-0026 §6). Only meaningful with more
    /// than one nest: by default a failed nest or cursor is quarantined and its healthy siblings keep
    /// indexing and serving.
    #[arg(long)]
    pub fail_fast: bool,

    /// Address to bind the HTTP API to.
    #[arg(long, default_value = "127.0.0.1:8288")]
    pub listen: String,

    /// Override the nest's `rpc_urls` at runtime without editing the config (repeatable). These are
    /// tried first; the nest's configured endpoints remain as fallback. Point at your own node.
    #[arg(long)]
    pub rpc: Vec<String>,

    /// Index only this many blocks back from the tip (recent-history mode). Explicitly overrides a
    /// nest's vendored `start_block`s. Omit to backfill from deployment when the nest declares start
    /// blocks, else from a default recent window.
    #[arg(long)]
    pub backfill: Option<u64>,

    /// Backfill finalized history straight to Parquet (skip the hot store) before tip-following -
    /// much faster for a from-deployment backfill (RFC-0004). The near-tip window still uses the hot
    /// path; the IVM view is rebuilt from the sealed segments.
    #[arg(long)]
    pub seal_direct: bool,

    /// Concurrent window fetches during the seal-direct history backfill (overlaps RPC latency).
    /// Try 8-16 against your own node; keep low on rate-limited public RPC.
    #[arg(long, default_value_t = 1)]
    pub concurrency: usize,

    /// Override the `eth_getLogs` block-window (the chain default otherwise). For a *sparse* contract
    /// over a long backfill - few events across many blocks - a large window (e.g. 50000) turns tens
    /// of thousands of near-empty requests into a few, so a from-history backfill finishes in minutes.
    /// Keep it under your provider's max block-range for `getLogs` (many allow 100k+ when the result
    /// set is small); the concurrent backfill fails the range rather than auto-shrinking it.
    #[arg(long)]
    pub window: Option<u64>,

    /// Disable the built-in admin UI (`/_admin/`) entirely - no routes, for hosted deployments that
    /// front their own dashboard (RFC-0010 Part A). Off-localhost the UI requires `NUTHATCH_ADMIN_TOKEN`
    /// to be set AND each request to present it as `?token=…` (or it self-disables with a log line).
    #[arg(long)]
    pub no_admin: bool,
}

#[derive(Args)]
pub struct DoctorArgs {
    /// Endpoint(s) to probe. Repeatable. Omit to probe whatever the nest in `--dir` is configured to
    /// use - which is where "my backfill is slow" usually starts.
    #[arg(long)]
    pub rpc: Vec<String>,

    /// Nest directory to read `rpc_urls` from when no `--rpc` is given.
    #[arg(long, default_value = ".")]
    pub dir: String,

    /// Probe `eth_getLogs` width filtered to this address, rather than unfiltered. Closer to what a
    /// real nest asks for, and some endpoints cap an unfiltered query harder than a filtered one.
    #[arg(long)]
    pub address: Option<String>,
}
