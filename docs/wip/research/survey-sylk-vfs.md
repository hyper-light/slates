# Survey: sylk's copy-on-write VFS systems (Go) — input for the slates (Rust) design

Source tree surveyed: `/Users/adalundhe/Projects/sylk` (all paths below are relative to it).
Written for the implementer of slates: a hermetic, purely in-memory, copy-on-write VFS service
that must never touch disk, provision volumes in <50 µs, and look like a normal path to host tools.

Every claim carries a `file:line` reference. Where sylk's behaviour conflicts with slates' goals
(especially "100% memory-only") the conflict is called out explicitly with the marker **DISK**.

---

## 0. Orientation: what sylk actually has (and the three "VFS layers")

sylk does not have one VFS; it has four cooperating in-memory structures plus a process broker that
projects them to real tools. The task statement's "three layers" map onto the code as follows:

| Layer (task name) | Go type | File | Role |
|---|---|---|---|
| Ephemeral pipeline VFS | `PipelineVFS` | `core/versioning/vfs.go:157` | Per-agent-pipeline whole-file overlay (maps of `[]byte`), staged mods, falls through to a base (global overlay → workspace image → **DISK**) |
| In-memory versioned CoW VFS | `purevfs.Workspace` (+ `ChunkStore`, `chunkArena`) | `core/purevfs/workspace.go:120`, `core/purevfs/storage.go:98`, `core/purevfs/chunk_arena.go:106` | Snapshot/branch tree with structural sharing, content-addressed chunks in an off-heap arena; imported once per workspace root as a "workspace image" (`core/versioning/workspace_image.go:62`) |
| Tool VFS (execution projection) | `projectedRoot` + `memoryNamespace` + FUSE brokers | `core/purevfs/process_broker_common.go:35,75`, `process_broker_linux_*.go`, `process_broker_darwin.go`, `process_broker_cgofuse.go`, `process_broker_windows.go` | Per-command FUSE mount that composes workspace / toolchain / tmp / cache / home / out namespaces for a sandboxed child process |
| (extra) Audit replica chain | `ReplicaVFS` | `core/versioning/replica_vfs.go:37` | Per-merge overlay layered on the previous merge's overlay, parent-pointer chain, WAL-backed |
| (extra) Session orchestration | `SessionVFS` | `core/versioning/session_vfs.go:22` | Owns global overlay, merge pipe (OT), semantic WAL, control WAL, commit queue, commit resolver, disk flusher |

Important vocabulary used by sylk docs and code:
- **green** = the session's global overlay (`SessionVFS.globalVFS`, a `PipelineVFS` created by `NewGlobalVFS`, `vfs.go:1180`). Pipelines merge "into green".
- **Copy** = a merged version pinned by the commit queue/retention; **water line** = the version below which Copies may be released (`copy_retention.go`).
- **workspace image** = the disk tree imported into a `purevfs.Workspace` (`workspace_image.go:351`).
- **strict-no-disk / strict-RAM mode** = broker execution with all scratch in RAM (`execution.go:22`), and session mode with no `StorageRoot` (`session_vfs.go:59`).

Note on a misleading filename: `core/purevfs/catalog.go` is **not** a VFS catalog. It is a language/toolchain runtime catalog (`RuntimeProfile`, `RuntimePolicy`, `PlanExecCell`, `catalog.go:28-84,178`) that maps languages to env vars (TMPDIR, GOCACHE, CARGO_HOME, …) and detects the project's language by walking the real disk (`catalog.go:248-360`, `filepath.WalkDir` at `catalog.go:342` — **DISK** read).

---

## 1. The VFS layers in detail

### 1.1 `purevfs.Workspace` — the real in-memory CoW filesystem

**Data structures** (`core/purevfs/workspace.go`):
- `Workspace` (`workspace.go:120-138`): one `sync.RWMutex` guards everything: `snapshots map[SnapshotID]*Snapshot`, `branches map[string]SnapshotID`, `handles map[uint64]*OpenFile`, an append-only `journal []JournalEntry`, and monotonic counters `nextSnapshot/nextInode/nextHandle/nextJournal`. A lazily allocated `branchGens` (`write_session.go:33`) holds per-branch atomic generation counters for fencing.
- `Snapshot` (`workspace.go:70-79`): immutable point-in-time view: `ID`, `Parent`, `Root *treeNode`, `Inodes *immutable.Map[uint64,*Inode]` (a HAMT from `github.com/benbjohnson/immutable`), `CreatedAt`, `Reason` (free text like `"write /a/b"`), `JournalLo/Hi`. The comment at `workspace.go:57-69` states the memory model: with S snapshots and N inodes, O(N + S·log N) instead of the previous O(N·S) full-clone design.
- `treeNode` (`workspace.go:115-118`): `Inode uint64` + `Children map[string]*treeNode`. The directory tree is a plain mutable-looking tree that is **path-copied** on mutation (`clonePathTo`, `workspace.go:1053-1068`): every node from the root to the mutated parent is shallow-cloned (`cloneTreeNode` copies the children map, `workspace.go:1039-1051`), siblings are shared by pointer.
- `Inode` (`storage.go:841-849`): `ID, Kind(File/Dir/Symlink), Mode, ModTime, Nlink, Body *FileBody, LinkTarget`. `Inode.Clone()` deep-copies the `FileBody` extent slice (`storage.go:851-860`).
- `FileBody` (`storage.go:417-420`): `size` + sorted `[]Extent`. An `Extent` (`storage.go:369-377`) is `{LogicalStart, Length, Kind, BlobHash, BlobOffset, RealPath, RealOffset}` with three kinds: `ExtentBlob` (content-addressed chunk), `ExtentRealFile` (**DISK**: a byte range of a real host file, read lazily through `RealFileReader.ReadAt`, `storage.go:402-415`, `os.Open` per call), `ExtentZero` (holes).
- `OpenFile` (`workspace.go:103-113`): a handle holds `BaseSnapshot`, `BaseInode`, `Writable`, `Dirty`, and a **private clone of the FileBody**.

**How a file is represented**: name → `treeNode.Inode` → `Snapshot.Inodes.Get(id)` → `Inode.Body` → extents → chunk hashes → `ChunkStore` shard → arena slot bytes (`workspace.go:1000-1010`, `storage.go:517-581`).

**How a version/snapshot is represented**: every mutation produces a brand-new `Snapshot` with `Parent = previous` (`finalizeSnapshotLocked`, `workspace.go:936-949`) and moves the branch head (`applyMutation`, `workspace.go:839-860`). So "versions" are per-mutation snapshots on a branch; `CreateBranch(name, base)` / `ForkBranch(src,dst)` (`workspace.go:208-233`) create O(1) branches that share everything.

**CoW granularity — precisely**:
1. Directory tree: per-path (root→parent chain is cloned, the rest shared). `workspace.go:1053-1068`.
2. Inode table: per-inode entry in the HAMT (only touched inode IDs get new entries; trie nodes on the path to them are copied by the library). `workspace.go:394-402, 914-931`.
3. File content: per **write call**, not per chunk. `FileBody.WriteAt` (`storage.go:583-603`) puts the *entire written buffer* as **one** chunk (`blobReplacement`, `storage.go:753-770` → `store.Put(content)`) and splices it into the extent list (`replaceRange`, `storage.go:659-695`). Only `NewChunkedFileBody` (`storage.go:441-472`) splits into fixed `chunkSize` pieces (default 64 KiB, `storage.go:449`), and it is used only on image import / whole-file write from the versioning layer (`workspace_image.go:396,572`, chunk size `workspaceImageChunkSize = 64<<10`, `workspace_image.go:26`). Consequence: a 10 MiB `WriteFile` creates one 10 MiB oversize arena region (see §2) — no dedup against the previous version, no re-chunking.
4. `Workspace.WriteFile` always builds a fresh empty body and `WriteAt(0, content)` (`workspace.go:416-422`): whole-file replacement, previous extents discarded.
5. Handle writes (`WriteHandle`, `workspace.go:770-786`) mutate the handle's private body clone; `CloseHandle` (`workspace.go:806-837`) re-validates that the branch head's entry is still `entriesEquivalent` to the base (`workspace.go:1116-1163`, compares inode id/mode/nlink and extent-by-extent) and otherwise returns `ErrHandleConflict` (`workspace.go:27`) — optimistic concurrency at close time, no merge.

**Chunk lifetime bug/limit worth knowing**: `ChunkStore.Put` sets `refs=1` on first insert and increments on dedup hits (`storage.go:160-166,189,197`), but **nothing in `workspace.go` ever calls `ChunkStore.Release`** (grep of `core/purevfs` shows `Release` only on budgets/guards: `process_broker.go:77`, `execution_governor.go:128,197,199`, `process_broker_common.go:456,863`). `FileBody.Clone` does not `Acquire` either (`storage.go:489-499`). So refcounts are decorative in production: chunks are freed only when the whole `ChunkStore` is closed (`storage.go:348-359`) and snapshots are never garbage-collected (`snapshots` map only grows, `workspace.go:947`; the `journal` slice only grows, `workspace.go:963`). Memory is bounded only by the store's `memoryLimit` (`storage.go:132-144`) which rejects with `ErrMemoryPressure`.

**Journal**: every mutation appends a `JournalEntry{Seq, Branch, Snapshot, Op, Path, SecondaryPath, Inode, Timestamp}` (`workspace.go:46-55, 951-972`); `JournalSince(seq)` copies the tail (`workspace.go:278-291`). Ops: create/modify/delete/rename/mkdir/symlink/link/seed/truncate (`workspace.go:34-44`).

**Hard links & symlinks**: `Link` increments `Nlink` and shares the inode ID between two tree nodes (`workspace.go:614-652`); `Symlink` stores `LinkTarget` (`workspace.go:654-688`); `Delete`/`Rename` decrement `Nlink` and delete the inode only at 0 (`workspace.go:501-508, 593-600`). Rename of a directory moves the subtree pointer (`workspace.go:603-606`) and rejects moving a dir into itself (`workspace.go:558-560`).

**What touches disk in this layer**: only `ExtentRealFile` reads (`storage.go:408-415`, via `SeedRealFile`, `workspace.go:428-431`) — nothing writes. The arena uses anonymous `mmap` (no file) (`chunk_arena_unix.go:26`).

**Write sessions / fencing** (`core/purevfs/write_session.go`): `BeginWriteSession(branch)` captures the branch generation (`write_session.go:97-113`); every write does one atomic load (`checkLive`, `write_session.go:142-150`); `Seal()` pins the head snapshot and bumps the generation so all other sessions on that branch fail with `ErrStaleGeneration` (`write_session.go:186-209`). There is no lease/timeout, and a session is single-goroutine only (`write_session.go:82-85`). This is an ownership-handoff primitive, not durability (no disk).

### 1.2 Workspace image — how a real directory becomes a `Workspace`

`core/versioning/workspace_image.go`:
- `scanWorkspaceImage` walks the disk root with `os.ReadDir` (**DISK** read), honouring `.gitignore` and `.git/info/exclude` via go-git's matcher (`workspace_image.go:174-285`), skipping `.git/.hg/.svn/.sylk` roots (`workspace_image.go:38-43`), and hashes every file's bytes into a SHA-256 "signature" (`hashWorkspaceFile`, `workspace_image.go:341-349`). Hard limit `SYLK_WORKSPACE_IMAGE_MAX_BYTES`, default 1 GiB (`workspace_image.go:25-27,159-172`); exceeding it returns `ErrWorkspaceImageTooLarge` (`workspace_image.go:322-325`).
- `importWorkspaceImage` reserves `totalBytes` from the global memory governor (`memorybudget.ScopeWorkspaceImage`, `workspace_image.go:352`), creates `purevfs.NewChunkStore(limit)` + `NewWorkspace` and writes every file as a 64 KiB-chunked body on branch `main` (`workspace_image.go:356-402`). Note the *whole file is read into a heap `[]byte` first* (`os.ReadFile`, `workspace_image.go:392`) then copied into arena slots.
- A process-wide registry keyed by `root + "\x00" + signature` refcounts images across sessions (`workspace_image.go:77-157`); `Release` closes the workspace (unmapping the arena) at refcount 0 (`workspace_image.go:148-155`). Two concurrent `Acquire`s of the same key may both import; the loser is discarded (`workspace_image.go:114-121`) — wasted work, not a correctness bug.
- `workspaceImageFS` gives the session a `vfsBaseFS` view (`ReadFile/Exists/ListDir/Stat`) pinned at `image.snapshot` (`workspace_image.go:415-480`). On first mutation it forks a branch `session-<n>` (`ensureBranch`, `workspace_image.go:523-535`) and `ApplyModifications` writes whole files with `NewChunkedFileBody` then moves the pinned snapshot to the branch head (`workspace_image.go:502-612`). This is the only place the CoW workspace is actually mutated by the versioning layer.

### 1.3 `PipelineVFS` — the ephemeral per-pipeline overlay (and "green")

`core/versioning/vfs.go`:
- State (`vfs.go:157-176`): `sync.RWMutex` + maps keyed by absolute path: `stagedContent map[string][]byte` (writes), `deletedPaths`, `workingContent` (seeded base copies), `visiblePaths` (sparse-workspace allow-list), `createdDirs`, `modifications map[string]*FileModification` (`vfs.go:31-41` — carries `NewContent` **and** `OldContent`, so every modified file is held ~3× in RAM: staged, mod.NewContent, mod.OldContent), plus `baseFS vfsBaseFS`, `baseReader func`, and a `memorybudget.Reservation` (`ScopeOverlay`).
- Read resolution order (`readContent`, `vfs.go:222-258`): deleted? → staged → working → `baseFS.ReadFile` → `baseReader` → **DISK** `os.ReadFile` (`readFromDisk`, `vfs.go:267-276`). `Exists`/`Stat`/`List` have the same fall-through, ending in `os.Stat`/`os.ReadDir` (`vfs.go:586-595, 643-653, 1236, 1265`). The global overlay gets `SetBaseFS(workspaceImageFS)` (`session_vfs.go:274`) so in practice green reads hit the in-memory image, but the disk fallbacks remain live code paths whenever `baseFS == nil`.
- Write path (`stageWrite`, `vfs.go:333-355`): snapshots the previous per-path state (`captureOverlayState`, `vfs.go:1049-1061`, which clones all byte slices), stores clones of content, then `commitOverlayState` recomputes the *total* overlay bytes by iterating every map (`overlayMemoryBytesLocked`, `vfs.go:1091-1106`, O(files) per write) and `Resize`s the reservation; on budget failure it restores the previous state and returns `ErrMemoryBudgetExceeded` (`vfs.go:1063-1070`).
- Transactions (`pipelineTx`, `vfs.go:1338-1490`): one at a time (`ErrVFSInTransaction`), staged in tx-local maps, `Commit` replays into `stageWrite/stageDelete`; not isolated from concurrent direct writes.
- `Diff` is a stub returning empty hunks (`vfs.go:830-854`).
- `ResetOverlay` (`vfs.go:1021-1033`) exists for "after a disk flush when the overlay is no longer needed (disk is source of truth)" — the design assumes **DISK** is the durable base.
- Sparse workspaces: `AllowedPaths` (`vfs.go:699-701`) restricts the pipeline to declared files; `RegisterVisiblePath`/`SeedFile` (`vfs.go:941-971`) pre-populate.

### 1.4 `memorySnapshotFS` — the older whole-tree RAM copy

`core/versioning/basefs.go:59-303`: `loadMemorySnapshotFS(root)` `filepath.WalkDir`s the root and `os.ReadFile`s **every file** into `map[string]*snapshotFile` (**DISK** read, full copy, no chunking, no dedup). It implements `ApplyModifications`. It is still compiled but `sessionBaseFS` now uses the workspace image instead (`session_vfs.go:464-471`); `DiskFlusher` accepts either as `SnapshotFS` (`session_vfs.go:288`).

### 1.5 `ReplicaVFS` — per-merge audit overlays chained by parent pointer

`core/versioning/replica_vfs.go`:
- Structure (`replica_vfs.go:37-140`): `sync.RWMutex`; `parent atomic.Pointer[ReplicaVFS]`; `diskBase vfsBaseFS` (the session's image FS); `writes map[string][]byte`; `deletes`; `owned`; an LRU `readCache` (`container/list`) sized `pathBudget × 8` (`auditReadContextFanout`, `replica_vfs.go:438,451-457`) where `pathBudget` = this merge's touched-path count + all ancestors' (`replica_vfs.go:213-220`); `chainReads` set (paths consulted from ancestors, for re-audit decisions); `sealed`/`abandoned` `atomic.Bool`; `wal *ControlWAL`.
- Read resolution (`replica_vfs.go:315-369`): local writes → read cache → iterative chain walk (`readThroughChain`, `replica_vfs.go:577-592`) with three-state result (hit / shadowed-by-delete / miss) → `diskBase.ReadFile`. Every hit copies bytes (defensive copies everywhere: `replica_vfs.go:388-390, 475-478, 496-497, 604-607`).
- Chain compression (`liveParent`, `replica_vfs.go:263-271`): a lock-free union-find style CAS that re-parents past abandoned layers; correctness argued from monotonicity (abandonment is one-way; parents only move up). `ChainDepth` (`replica_vfs.go:278-289`) is the observability gauge.
- Writes/deletes (`replica_vfs.go:785-873`) hold the mutex across the **ControlWAL append (fsync, DISK when a StorageRoot is configured)** then mutate. `Seal` snapshots the diff (`replica_vfs.go:884-934`), `Abandon` frees the overlay immediately (`releaseOverlayLocked`, `replica_vfs.go:534-539, 939-974`). Both flip in-memory state first and report `ErrControlMarkerNotDurable` if the WAL marker fails (`replica_vfs.go:191-199`).
- Rehydration from the control WAL (`ApplyControlEntry`, `replica_vfs.go:979-1020`; driven by `session_vfs.go:1287-1332`).
- `Stat` for overlay-only paths synthesizes `0644` + now (`replica_vfs.go:671-677`); `ListDir` walks the chain recursively and unions (`replica_vfs.go:685-763`).

### 1.6 `SessionVFS` — the orchestrator (what owns what)

`core/versioning/session_vfs.go:22-122` lists the members. `NewSessionVFS` (`session_vfs.go:256-462`) builds, in order: OT engine; semantic WAL (`openSessionSemanticWAL`, disk or memory); workspace image + `workspaceImageFS`; green `PipelineVFS` with `SetBaseFS(image)`; `MergePipe` (background goroutine); `DiskFlusher`; legacy blob/DAG/oplog/WAL stores for the CVS shim; `VFSManager`; a `GoroutineScope`; `DefaultCVS`; `ControlWAL` **iff `StorageRoot != ""`** (`session_vfs.go:352-359`, otherwise nil = "strict-RAM mode"); `CommitQueue(ctrlWAL)`; `CopyRetention`; `ReplicaLifecycleLog`; replays the semantic WAL into green (`replayDraftFromWAL`, `session_vfs.go:1999-2028`) and the control WAL (`replayControlWAL`, `session_vfs.go:1185-1277`); starts the `CommitResolver`; starts the `DispatchGate` (backpressure on `BeginPipeline`, default derived from host by `DeriveDispatchMaxInFlight`, `session_vfs.go:435-460`).
- `BeginPipeline` (`session_vfs.go:493-596`): creates a `PipelineVFS` through `VFSManager`, pins its base version at the WAL head (or an explicit `BaseCopyVersion` for remediation), installs `baseReader = globalVFS.Read` and `baseFS = pipelineOverlayBaseFS{green}` (`session_vfs.go:551-555`), registers with the merge pipe, retains the Copy.
- `MergePipelineIntoGreen` (`session_vfs.go:650-775`): extracts persistent mods, calls `mergePipe.Merge` (OT), closes the pipeline, records a `MergeDescriptor`, enqueues on the commit queue (write-ahead to the control WAL; failure aborts the merge), fires synchronous merge callbacks (audit-replica spawn).
- `BeginAuditReplicaVFS` (`session_vfs.go:1501-1560`) walks the merge log backwards for the newest live replica as parent, retains the parent's Copy; release paths at `session_vfs.go:1597-1707` (water-line driven, with an optional `ForensicArchiver` hook — **DISK** if the operator's archiver writes).
- Locking: one `sync.Mutex` (`s.mu`) for the maps, a separate `closeMu`, `mergeCallbackMu`, `draftMu` shared with MergePipe/DiskFlusher; callbacks are fired outside `s.mu` (`session_vfs.go:1883-1889`).

(§1.7 "how merges and commits flow" is in Section 4 below, together with the WAL/commit-queue/flusher.)


### 1.7 How merges and commits flow (end to end)

1. An agent writes through a `FileAccess` into its `PipelineVFS` (whole-file staged bytes, §1.3).
2. `SessionVFS.MergePipelineIntoGreen` (`session_vfs.go:650`) pulls `GetModifications()` filtered to persistent ones (`persistentFileModifications`, `session_vfs.go:942`) and calls `MergePipe.Merge` (`merge_pipe.go:147`), which hands the request over a channel (buffer 64, `merge_pipe.go:13`) to **one merge goroutine** (`mergeLoop`, `merge_pipe.go:195-208`). The caller blocks on a result channel; the context passed is `WithoutCancel` (`session_vfs.go:686, 2054-2059`).
3. `executeMerge` (`merge_pipe.go:221-258`) takes `draftMu` (shared with `DiskFlusher`), then:
   - `transformMods` (`merge_pipe.go:263-294`): reads every semantic-WAL delta since the pipeline's base version (`wal.GetDeltasSince`) — with the disk WAL that is one `os.ReadFile` + JSON decode per entry (`versioned_wal.go:132-151, 303-309`, **DISK**) — and turns both sides into byte-offset insert/delete operations via `diffmatchpatch` on the **whole old/new content** (`merge_pipe_precise.go:26-74`), then OT-transforms each op against the accumulated ops for the same path (`merge_pipe_precise.go:97-140`, `OTEngine.Transform`, conflicts routed to a `ConflictResolver`, default no-op).
   - `applyToGlobal` (`merge_pipe.go:298-313`): reads the current green content, applies the transformed ops byte-wise (`applyOperationsToContent`, `merge_pipe_precise.go:223-246`, allocating a new slice per op), writes the full result into green with `PipelineVFS.Write`, and emits one `WALFileDelta{Path, Op, NewContent, OldContent}` per file (`merge_pipe_precise.go:146-199`) — full old **and** new contents.
   - `wal.AppendDelta(pipelineID, deltas)` bumps the minor version (`versioned_wal.go:88-99` / memory variant `memory_versioned_wal.go:27-37`). On disk this is a temp-file write + rename of a JSON entry **without fsync** (`versioned_wal.go:287-301`) plus an `index.json` rewrite on every append (`versioned_wal.go:270-275, 400-418`) — **DISK**.
4. Back in `MergePipelineIntoGreen`: pipeline VFS closed, `MergeDescriptor` recorded in the in-memory `mergeLog` (`merge_descriptor.go:96-113`), descriptor **enqueued on the CommitQueue** (write-ahead to the control WAL with fsync, `commit_queue.go:219-248`, `control_wal.go:140-165` — **DISK** when a StorageRoot exists), and merge callbacks fire synchronously (`session_vfs.go:766`).
5. Audit replicas (inspector + tester) get a shared `ReplicaVFS` (`BeginAuditReplicaVFS`, §1.5), read through the chain, and eventually `MarkAccepted`/`MarkRejected` on the queue (`commit_queue.go:294-362`). A sealed replica's diff is resubmitted through the same MergePipe as an "audit addendum" pipeline (`SubmitAuditAddendum`, `session_vfs.go:1766-1891`).
6. The `CommitResolver` goroutine (`commit_resolver.go:34`, woken by queue events with a 100 ms ticker fallback, `commit_resolver.go:93-96, 161-183`) processes the head strictly FIFO (`processHead`, `commit_resolver.go:226-315`): Accepted → optional user confirmation gate (`flushConfirmed`, `commit_resolver.go:325-363`) → `DiskFlusher.Flush` → `MarkCommitted` → `Advance` → `advanceWaterLineTo` (retention GC + control-WAL compaction, `commit_resolver.go:490-515`). Rejected heads block the queue until superseded or abandoned.
7. `DiskFlusher.Flush` (`disk_flusher.go:62-111`) is **whole-overlay**, not per-descriptor (`commit_resolver.go:463-480` says so explicitly): under `draftMu` it stages every persistent mod as a temp file next to the target (`os.CreateTemp` + write + `Sync`, `disk_flusher.go:230-247`), renames them into place (`disk_flusher.go:249-260`), rolls back on failure by rewriting old contents (`disk_flusher.go:194-219`), appends a **checkpoint** (major version bump) to the semantic WAL containing full old+new contents (`disk_flusher.go:91`), advances the RAM base (`snapshotFS.ApplyModifications`, i.e. the workspace image branch, `disk_flusher.go:99-103`), then `ResetOverlay` and re-applies ephemeral (command-execution) mods (`disk_flusher.go:105-109`). Flushing is refused with `ErrDiskExportDisabled` unless `AllowDiskExport` (`disk_flusher.go:63-65`) — this is the only switch that makes a session truly write-free towards the project tree.

---

## 2. Chunk arena and memory compression

### 2.1 Chunk arena (`core/purevfs/chunk_arena.go`, `chunk_arena_unix.go`, `chunk_arena_windows.go`)

- Purpose: off-heap, RAM-only backing store for `ChunkStore` bytes so chunk payloads neither get GC-scanned nor show up in heap profiles (`chunk_arena.go:11-32`).
- **Allocation strategy**: size-classed slabs with per-class free lists. Five classes `4 KiB, 16 KiB, 64 KiB, 256 KiB, 1 MiB` (`arenaSizeClasses`, `chunk_arena.go:53-59`); `pickClass` is a linear scan for the smallest class ≥ size (`chunk_arena.go:237-244`). Each slab holds a fixed `slabSlotsPerClass = 16` slots (`chunk_arena.go:67`), so a 64 KiB-class slab is 1 MiB and a 1 MiB-class slab is 16 MiB of virtual memory mapped on first use of that class (`growLocked`, `chunk_arena.go:338-350`). Free slots are pushed to a LIFO `freeIdx []slotRef` (`chunk_arena.go:256, 298-299, 312-319`). Slabs are never unmapped until `Close` (confirmed by the test at `chunk_arena_test.go:239-251`: "Mapped bytes do NOT shrink on release").
- **Oversize**: anything > 1 MiB gets its own `mmap` region sized exactly to the request and unmapped on free (`oversizeArena`, `chunk_arena.go:352-429`). Because `FileBody.WriteAt` stores the whole write as one chunk (§1.1), every file write > 1 MiB is an oversize mapping — one `mmap`/`munmap` syscall pair per write.
- **Chunk size choice**: "derived from the codebase's chunk-size anchor (NewChunkedFileBody default = 64 KiB)" (`chunk_arena.go:25-28`); `NewChunkedFileBody` defaults `chunkSize` to `64<<10` when ≤ 0 (`storage.go:448-450`); the versioning layer passes `workspaceImageChunkSize = 64<<10` (`workspace_image.go:26`). The number is a code constant, not measured from data. Slot slices are capped at the slot capacity (`slab.bytes[start : start+size : start+c.slotSize]`, `chunk_arena.go:308`).
- **Unix vs Windows**: unix uses `unix.Mmap(-1, 0, size, PROT_READ|PROT_WRITE, MAP_ANON|MAP_PRIVATE)` with a heap fallback when mmap fails (`chunk_arena_unix.go:22-34`) and `Munmap` on release (`chunk_arena_unix.go:38-43`); Windows always uses `make([]byte, size)` on the Go heap and release is a no-op (`chunk_arena_windows.go:12-20`), losing the off-heap benefit (comment says VirtualAlloc was considered "not load-bearing").
- **Page-size awareness**: none. Slab sizes are powers of two multiples of the class size, which happen to be page multiples on 4 KiB pages, but the code never queries `os.Getpagesize()`; there is no `madvise(MADV_FREE/DONTNEED)` on free (the docs propose it: docs corpus §G "anonymous mmap arenas with MADV_FREE/DiscardVirtualMemory").
- **Reference counting**: lives in `ChunkStore`, not the arena. `chunkEntry{slot *chunkSlot; refs atomic.Int32}` (`storage.go:70-73`); Put → refs=1 / dedup hit → `refs.Add(1)` under a shard **read** lock (`storage.go:157-166`); `Release` takes the shard write lock and frees the slot when `refs <= 1` (`storage.go:217-236`). As noted in §1.1, production code never calls `Release`/`Acquire`, so slot reuse only happens in tests.
- **Freeing/teardown**: `chunkArena.Close` unmaps all slabs and oversize regions, `runtime.SetFinalizer` is a safety net (`chunk_arena.go:121-135, 220-233`); `ChunkStore.Close` nils the shard maps first (`storage.go:348-359`). After Close any surviving slice into a slab is a dangling pointer into unmapped memory (`chunk_arena.go:213-219` admits "reading slot.Bytes() will reference unmapped memory and crash the process").
- **Locking**: one `sync.Mutex` per size class + one for the oversize list + one for the stats struct (`chunk_arena.go:113-119, 254, 358`) — every allocation takes at least two mutexes (class + stats). `ChunkStore` is sharded 256 ways by `hash[0]` with `RWMutex` per shard (`storage.go:75-96, 123-125`); the global byte budget is an atomic add/rollback (`storage.go:132-148`). `Stats()` walks all 256 shards and all entries (`storage.go:304-324`).
- **Hashing**: SHA-256 over the full chunk on every `Put` (`storage.go:29-31, 154`).
- **Benchmarks present but no recorded numbers**: `BenchmarkChunkStore_PutUniqueChunks` (64 KiB unique puts), `_PutDedupedChunks`, `_CopyInto` (`chunk_arena_test.go:425-467`) — no results are committed anywhere in the repo (grep for `ns/op` in tests/docs found nothing).

### 2.2 Memory compression (`core/purevfs/memory_compression.go`)

- Scope: only the broker's **memory namespaces** (`/tmp`, `/cache`, `/home`, `/out` inside a sandboxed command; `memoryNamespace`, `process_broker_common.go:75-93`). It does **not** apply to the `Workspace`/chunk arena or any versioning structure.
- Algorithm: DEFLATE at `flate.BestSpeed` (level 1) per whole file (`flateCompress`, `memory_compression.go:146-165`); `buf.Grow(len/2)` presizes the buffer assuming 2:1 (`memory_compression.go:152`). Compressed payload replaces `content` in place; `rawSize` records the logical size (`process_broker_common.go:82-103`). Results ≥ raw size are discarded (`memory_compression.go:53-58`).
- Trigger: only when a reservation against the per-run execution budget fails (`adjustFileBytesLocked`, `process_broker_common.go:825-850`) or when inflating a compressed file for a read would exceed the budget (`maybeInflateInPlaceLocked`, `memory_compression.go:105-144`). Candidates are all files sorted by `accessTime` ascending (`compressionCandidatesLocked`, `memory_compression.go:73-87`, O(n log n) sort under the namespace **write** lock on every pressure event). Reads take the namespace write lock because they may inflate (`process_broker_common.go:716-730`).
- Claimed ratios/CPU: comment claims "~2-3x compression on typical text and binaries … at ~500 MB/s on commodity CPUs" (`memory_compression.go:146-149`) and the test comment claims "DEFLATE on 300 bytes of a single byte hits ~6x" (`memory_compression_test.go:48-49`). No benchmark measures either; tests only assert that compression happened / round-trips (`memory_compression_test.go:27-279`).
- Failure mode: if compression cannot free enough, `ErrMemoryPressure` propagates and the FUSE layer returns `ENOMEM` (`translateExecutionFSError`, `process_broker_common.go:993-994`) — "No silent spill to disk" (`memory_compression_test.go:232-235`).

---

## 3. Process brokers, mounting, execution broker/governor, host read cache

### 3.1 Execution planning (`core/purevfs/execution.go`)

- Modes: `compatibility` vs `strict-no-disk` (`execution.go:18-23`); strategies `direct-passthrough`, `process-broker`, `go-overlay-manifest`, `workspace-materialize` (`execution.go:34-41`). Only `process-broker` is memory-only; `workspace-materialize` and `go-overlay-manifest` explicitly "write compatibility artifacts" to disk (`execution.go:324-332`, **DISK**).
- Every plan carries six mount specs: workspace (read-only or overlay-source), and in-memory `/tmp`, `/cache`, `/home/agent`, `/out` (`DefaultLogicalExecRoots`, `execution.go:185-197`; mounts `execution.go:244-289`), plus one read-only toolchain mount per binary at `/.sylk/toolchain/bin/<tool>` (`execution.go:296-308`, `process_broker.go:13`). `PATH` is rewritten to the toolchain root.
- Language detection for the plan walks the real project dir (`catalog.DetectProject`, `execution.go:210-213`).

### 3.2 The projection (`core/purevfs/process_broker_common.go`)

- `projectedRoot` (`process_broker_common.go:35-43`) holds ordered `mountedNamespace`s (longest prefix first, `process_broker_common.go:131-133`), synthetic static parent dirs, an `executionBudget`, and a `handles map[uint64]*projectedHandle` under **one mutex**. `findNamespace` is a linear prefix scan per operation (`process_broker_common.go:467-483`).
- Namespaces: `workspaceNamespace` (delegates to the `ExecutionFS` supplied by the versioning layer, i.e. the pipeline/global overlay, `process_broker_common.go:532-598`; rename = read+write+delete), `hostNamespace` (read-only real-disk passthrough through the host read cache, `process_broker_common.go:600-642`, **DISK** reads), `memoryNamespace` (flat `map[path]*memoryFile` + `map[path]*memoryDir`, whole-file `[]byte`, `RWMutex`; `ListDir`/`hasChildren`/rename walk **all** files by string prefix, `process_broker_common.go:666-688, 811-823, 903-922`).
- Handles: every open loads the **entire file into a private heap buffer** (`loadHandleContent`, `process_broker_common.go:388-407`), reserving its size from the budget; writes grow/copy the buffer (`resizeBytes`, `process_broker_common.go:1018-1023`, copy on shrink), and `Flush`/`Release` write the whole buffer back with `namespace.WriteFile` (`process_broker_common.go:377-386`). Reads take `p.mu` to find the handle then copy without a lock (`process_broker_common.go:273-288`) — a concurrent write to the same handle races on `handle.content`.
- Error mapping to errno at `process_broker_common.go:988-1007` (`ErrMemoryPressure→ENOMEM`, not-exist→`ENOENT`, exist→`EEXIST`, permission→`EPERM`).

### 3.3 Linux (`process_broker_linux_common.go`, `_bazil.go`, `_hanwen.go`)

- **Why two FUSE libraries**: `bazil.org/fuse` is the default build; `hanwen/go-fuse/v2` is selected with `-tags substrate_fuse_v2` (`process_broker_linux_common.go:3-21`). Rationale in `process_broker_linux_hanwen.go:13-25`: bazil "functionally unmaintained (last release activity ~16 months stale)", hanwen "tracks the FUSE wire protocol through 7.12.28, ships passthrough mode … in production at Tailscale, GCSFuse"; bazil "will be retired one release after substrate_fuse_v2 becomes the default". Docs corpus §B confirms the migration plan (TOOL_VFS.md:1040-1046).
- **Preconditions** (`strictExecutionProbe`, `process_broker_linux_common.go:59-70`): `bwrap` and `fusermount3` on PATH, and `/dev/shm` must be tmpfs (`Statfs … TMPFS_MAGIC`, `process_broker_linux_common.go:106-112`). No root needed (unprivileged FUSE + bubblewrap user namespaces).
- **Mount point**: `os.MkdirTemp("/dev/shm", "sylk-execfs-*")` (`process_broker_linux_common.go:95-104`) — a directory in RAM-backed tmpfs, not on disk.
- **Mount options**: bazil: `FSName("sylk-purevfs")`, `Subtype("sylk")` (`process_broker_linux_bazil.go:57-61`), every open gets `OpenDirectIO` (`_bazil.go:165,179`) so the kernel page cache is bypassed. hanwen: `EntryTimeout`/`AttrTimeout` = 100 ms (`_hanwen.go:156-165`), `AllowOther:false`, `DirectMount:false` (goes through fusermount3), `DisableXAttrs:true` (`_hanwen.go:140-154`), `FOPEN_DIRECT_IO` on open/create (`_hanwen.go:287,295`). Inode numbers are FNV-64a of the path (`inodeForPath`, `process_broker_linux_common.go:114-118`) — rename changes the inode number, hard links are impossible.
- **Sandbox**: `bwrap --unshare-all --share-net --die-with-parent --proc /proc --dev /dev`, `--bind`/`--ro-bind` of each projected mount from `<mountpoint>/<virtual>` to `<virtual>`, read-only binds of `/usr,/bin,/sbin,/lib,/lib64,/etc` plus every PATH dir, `--chdir` (`process_broker_linux_common.go:135-201`). The child therefore sees `/workspace`, `/tmp`, `/cache`… at their canonical paths inside its own mount namespace, backed by the FUSE mount.
- **Lifetime**: one mount per `Run` call: `Run` → `newProjectedRoot` → `mountExecutionRoot` → run → `defer mount.Close()` (`process_broker.go:66-88`). Close = `Unmount` + `conn.Close` + wait ≤ 250 ms for the serve goroutine + `os.Remove(mountpoint)` (`_bazil.go:81-102`); hanwen: `server.Unmount` + remove (`_hanwen.go:170-179`).
- **Crash cleanup**: none beyond `--die-with-parent` for the child. If the sylk process dies, the FUSE mount in `/dev/shm/sylk-execfs-*` is left for the kernel to drop when the fd closes; no stale-mount sweep exists (grep found no `fusermount -u` recovery code).
- **Implemented ops**: lookup/readdir/mkdir/unlink/rmdir/rename/create/open/setattr(size only)/read/write/flush/release (`_hanwen.go:94-110`). Missing: symlink, link, chmod/chown/utimens (ignored), xattrs, fsync, statfs, locks, `mmap` semantics (direct IO forbids shared mmap on many kernels).

### 3.4 macOS (`process_broker_darwin.go`, `darwin_fuse_backend.go`, `process_broker_cgofuse.go`, `process_broker_darwin_nocgo.go`)

- `darwin_fuse_backend.go` is only a **classifier**: it checks for `/Library/Filesystems/macfuse.fs` or `osxfuse.fs` (macFUSE, a kernel extension) and `/usr/local/lib/libfuse-t.dylib` or `/opt/homebrew/lib/libfuse-t.dylib` (FUSE-T, a kext-free NFS-based userland FUSE) (`darwin_fuse_backend.go:33-51`). macFUSE wins when both exist because the vendored `cgofuse` dlopen chain prefers it (`darwin_fuse_backend.go:53-75`; the patched cgofuse lives in `third_party/cgofuse/fuse/host_cgo.go`). So the answer to "what is it": **cgofuse (libfuse-compatible C API) over macFUSE or FUSE-T; not FSKit, not a custom NFS server.**
- Preconditions: `sandbox-exec` present, a FUSE backend installed, `/Volumes` exists (`process_broker_darwin.go:35-46`). Mount point `/Volumes/sylk-execfs-<unixnano>` with `-o volname=…` (`process_broker_darwin.go:165-173`). macFUSE needs the kext approved (admin, reboot on Apple Silicon); FUSE-T needs no privileges but has NFS semantics (weaker locking, no inotify).
- Mount readiness: `mountCGOFuseExecutionRoot` waits for `Init()` (`ready` channel) with a **2 s timeout** then unmounts (`process_broker_cgofuse.go:36-60`). Close = `host.Unmount()` + wait + `os.Remove(mountpoint)` (`process_broker_cgofuse.go:66-77`).
- Sandbox: a Seatbelt profile string passed to `sandbox-exec -p` (`darwinSandboxProfile`, `process_broker_darwin.go:107-135`): `(deny default)`, allow process/sysctl, read `/usr,/bin,/sbin,/Library,/System,/private/etc` + PATH dirs, read+write the mount root, network in/out. Note macOS has no mount namespaces, so argv/env/cwd are **rewritten** to point under the mount (`hostProjectedExecutionArgv`, `translatedExecutionEnv`, `process_broker.go:267-346`; shell `-c` strings get naive `strings.ReplaceAll` of virtual roots, `process_broker.go:339-346`). `sandbox-exec` is deprecated by Apple but still works.
- `darwin_nocgo` fallback: with `CGO_ENABLED=0` every strict call returns `ErrStrictExecutionUnavailable` and capabilities are empty (`process_broker_darwin_nocgo.go:11-25`); the planner then falls back to `direct-passthrough` (real disk) or fails (`execution.go:339-347`). Same for `process_broker_other.go` (BSD etc.).
- cgofuse op set: `Getattr/Mkdir/Unlink/Rmdir/Rename/Create/Open/Truncate/Read/Write/Flush/Release/Readdir/Access/Statfs` (`process_broker_cgofuse.go:79-196`); `Statfs` returns zeros; `Readdir` does not fill stat entries (extra `Getattr` per entry). All ops use `context.Background()`.

### 3.5 Windows (`process_broker_windows.go`)

- cgofuse over **WinFsp**: presence is detected by globbing `winfsp-*.dll` in `%ProgramFiles%\WinFsp\bin`, `%ProgramFiles(x86)%\…` and PATH (`process_broker_windows.go:37-78`). Mount point = the first free **drive letter** scanning `Z:` downwards (`newWindowsMountpoint`, `process_broker_windows.go:88-100`); no directory mount.
- Sandbox: the child runs with a restricted token (`DISABLE_MAX_PRIVILEGE|LUA_TOKEN|WRITE_RESTRICTED`) at low integrity (`process_broker_windows.go:155-218`) inside a Job Object with `ActiveProcessLimit=1`, kill-on-close, and a job/process memory limit equal to the per-run budget (`process_broker_windows.go:220-243`). Argv/env/cwd are rewritten like macOS. `cmd.exe /d /c` is the shell (`process_broker.go:105-108`).
- Chunk arena on Windows is heap-backed (§2.1).

### 3.6 Execution broker, governor, admission (`process_broker.go`, `execution_governor.go`, `execution_broker_instrumented.go`)

- `DefaultExecutionBroker` = `nativeExecutionBroker` wrapped in an Activity-Fabric span emitter (`process_broker.go:47-53`, `execution_broker_instrumented.go:19-84`; every run records argv, exit code, stdout/stderr byte counts).
- `Run` sequence (`process_broker.go:66-88`): validate → probe platform prerequisites → `governor.Admit()` → build projection → mount → run sandboxed command → unmount. There is **no concurrency limit** on simultaneous runs other than memory: `Admit` (`execution_governor.go:117-136`) reserves 0 bytes from the global `memorybudget` governor for scope `execution` and then `reserveBytes` = 16 MiB up-front (`defaultExecutionReserve`).
- Limits (env-overridable, `execution_governor.go:13-23`): total execution memory `SYLK_EXECUTION_MEMORY_MAX_BYTES` = 512 MiB, per-run `SYLK_EXECUTION_RUN_MAX_BYTES` = 256 MiB, reserve 16 MiB, captured output `SYLK_EXECUTION_OUTPUT_MAX_BYTES` = 8 MiB (stdout and stderr each, truncated with a flag, `executionCaptureBuffer`, `execution_governor.go:211-261`). The governor is one mutex for all runs (`execution_governor.go:30-37, 138-177`); every byte reserved by a memory namespace or a handle goes through it and additionally calls `memorybudget.Reservation.Resize` (another global mutex, `core/memorybudget/governor.go:111-169`). Global budget defaults: total 2 GiB, workspace-image 1 GiB, overlay 512 MiB, execution 512 MiB (`core/memorybudget/governor.go:24-27`).
- Env passed to the child is filtered to `PATH, LANG, TERM, TZ, LC_*, SYSTEMROOT, COMSPEC, PATHEXT` plus the plan's env (`keepExecutionEnvKey`, `process_broker.go:157-179`).

### 3.7 Host read cache (`core/purevfs/host_read_cache.go`)

- Process-wide LRU of **whole toolchain files** read through `hostNamespace` (interpreters, stdlib, node_modules, headers) keyed by absolute path, validated on every lookup by an `os.Stat` comparing size + mtime (`host_read_cache.go:19-24, 112-125, 174-194`, **DISK** stat per read, full `os.ReadFile` on miss). Budget 128 MiB default (`host_read_cache.go:52-58`), tunable via `SetHostReadCacheBudget`; single global `sync.Mutex`; eviction from the LRU tail until under budget (`host_read_cache.go:158-167`); entries larger than the budget are stored anyway (`host_read_cache.go:127-130`). Never applied to writable namespaces. Invalidation is purely stat-based; there is no watcher. Because FUSE reads copy from `handle.content` (a reference to the cached slice), a cache eviction does not free the bytes until the handle closes.

---

## 4. Write sessions, WALs, commit queue, disk flusher — ordering, durability, crash behaviour, and every disk touch

### 4.1 Ordering and durability model

- **Linearization point** for content: the single `MergePipe` goroutine + `draftMu` (`merge_pipe.go:31-46, 237-240`). All merges (pipeline or audit addendum) are applied to green one at a time, each producing exactly one semantic-WAL entry with a monotonically increasing `SemanticVersion` (minor bump per merge, major bump per disk checkpoint: `versioned_wal.go:97,110`).
- **Decision log**: the `ControlWAL` records every commit-queue / retention / replica-lifecycle / replica-overlay / session-epoch transition **before** the in-memory mutation, with `file.Write` + `file.Sync` under a mutex per append (`control_wal.go:135-165`). Entries are CRC-validated on open; a corrupt tail is truncated (`control_wal.go:84-133`). Entry encoding is in `control_entry.go` (`EncodeControlEntry`/`DecodeControlEntry`).
- **Content vs decision split**: the control WAL stores version keys, pipeline id, path count and the path list for queue entries (`commit_queue.go:225-231`); the full descriptor (certificate, timestamps) lives in the in-memory merge log and is looked up on replay, with a minimal-descriptor fallback (`commit_queue.go:668-699`); replica overlay writes, however, store the **full file content** in the control WAL (`replica_vfs.go:813-823`).
- **Disk commit order** = commit-queue arrival order; the resolver flushes at most one head at a time and blocks on a rejected head (`commit_resolver.go:21-29, 253-255`). But the flush itself writes the **union** of all persistent green modifications, so an accepted merge behind a rejected one is still written when the rejected head is superseded/abandoned (`commit_resolver.go:463-470, 381-385`).
- **Crash behaviour**: on open, `replayDraftFromWAL` rebuilds green from the latest checkpoint + deltas (`session_vfs.go:1999-2028`), then `replayControlWAL` rebuilds queue, retention, replica lifecycle, replica overlays and the session clock (`session_vfs.go:1185-1277`). The design claims crash and clean close are the same path (docs corpus §C, PGV:515). Gaps: semantic-WAL entry files are renamed into place without `fsync` (`versioned_wal.go:287-301`), so a power loss can leave `index.json` referencing an entry with no durable bytes; `Compact` on the versioned WAL removes files without syncing the directory (`versioned_wal.go:196-221`); the in-memory `VersionIndex` is capped at 1000 entries and older ones are simply dropped from the index though files stay (`versioned_wal.go:17-18, 320-326`) — `GetDeltasSince` on a version older than the index window silently returns fewer deltas.
- **Write-session fencing in purevfs** (§1.1) is orthogonal: it never touches a WAL.

### 4.2 Every place disk is touched (exhaustive list, with the switch that disables it)

| # | Site | Kind | Disabled by |
|---|---|---|---|
| 1 | `workspace_image.go:219,261,342-348,392` (`os.ReadDir`, `os.ReadFile`, `.gitignore` reads, hashing) | READ project tree on session open | never (this is how the image is built) |
| 2 | `basefs.go:91-128` (`memorySnapshotFS` walk + read) | READ whole tree | unused by default (`session_vfs.go:464-471`) |
| 3 | `vfs.go:267-276, 586-595, 643-653, 1236, 1265` (`os.ReadFile/Stat/ReadDir` fallbacks in `PipelineVFS`) | READ | only when `baseFS == nil` |
| 4 | `storage.go:406-415` (`OSRealFileReader`) via `SeedRealFile` | READ host file ranges | never used by versioning (no callers outside tests) |
| 5 | `catalog.go:342, 1206`, `detect.*` | READ project dir for language detection | never |
| 6 | `host_read_cache.go:175-192`, `process_broker_common.go:600-614` | READ toolchain files + stat | never (toolchain passthrough) |
| 7 | `process_broker_linux_common.go:99` (`MkdirTemp` in `/dev/shm`) | tmpfs dir (RAM) | n/a |
| 8 | `process_broker_darwin.go:169-172` (`/Volumes/...` mountpoint) | mount dir on the root volume (metadata write) | n/a |
| 9 | `versioned_wal.go:71, 213, 292-299, 304, 409-416` | WRITE semantic WAL entries + `index.json` (temp+rename, no fsync) | `StorageRoot == ""` → `MemoryVersionedWAL` (`session_semantic_wal.go:20-28`) |
| 10 | `control_wal.go:66-70, 157-161, 269-335` | WRITE control WAL (`control-wal/log.bin`, fsync per append; compaction temp+rename+dirsync) | `StorageRoot == ""` (`session_vfs.go:352-359`) |
| 11 | `disk_flusher.go:230-260, 375-403` | WRITE project files (temp+fsync+rename), mkdir, remove | `AllowDiskExport == false` (`disk_flusher.go:63-65`) |
| 12 | `disk_flusher.go:367-373` | READ old content for rollback/WAL delta | same as 11 |
| 13 | `session_vfs.go:1409-1415` steering journals under `StorageRoot/steering-journals` | WRITE (agents' journals) | `StorageRoot == ""` |
| 14 | `ForensicArchiver` (`session_vfs.go:220-253, 1655-1679`) | WRITE (operator-provided) | not configured |
| 15 | `execution.go:324-332` materialize / go-overlay strategies | WRITE compatibility artifacts | strict mode |
| 16 | legacy sandbox / staging paths documented in docs corpus §A (`~/.sylk/staging`, `~/.sylk/versions`, `~/.sylk/wal`, Seatbelt profile in `os.TempDir()`) | WRITE | legacy code outside the surveyed packages |

Conclusion for slates: sylk's "strict-RAM mode" exists (no StorageRoot, no disk export) but it is a **configuration**, not an invariant — the type system lets every layer fall through to `os.*`. The only component that is memory-only by construction is `purevfs.Workspace` + `ChunkStore` (except `ExtentRealFile`).

### 4.3 Legacy stores still constructed per session

`SessionVFS` still allocates a `MemoryBlobStore` (map of whole blobs keyed by SHA-256, `blob_store.go:22-64`), `MemoryDAGStore` (per-file version DAG with heads/children/file indices, `dag_store.go:27-42`), `MemoryOperationLog` (`operation_log.go:21-36`) and `MemoryWAL` for the CVS shim (`session_vfs.go:294-306`). These are the older per-file DAG design (`FileVersion` with multiple parents, vector clocks, variant groups, `file_version.go:7-21`) and are only reached through `CVS`/`VFSManager` history APIs. They copy every payload on every Get/Put (`blob_store.go:34-64`).

---

## 5. The API surface agents get, and what is missing for slates

### 5.1 The programmatic chokepoint: `FileAccess`

Every agent file operation goes through `versioning.FileAccess` (`core/versioning/file_access.go:52-72`), which embeds `ReadOnlyFileAccess` (`file_access.go:18-42`):

| Method | Notes |
|---|---|
| `ReadFile(ctx, path) ([]byte, error)` | whole file only; no offset/range at this layer |
| `Exists`, `Stat`, `ListDir`, `WorkingDir` | |
| `Glob(ctx, root, pattern, exclude)` | `**` handled by suffix hack (`file_access_disk.go:216-231`) |
| `Grep(ctx, root, pattern, include, contextLines, maxMatches)` | line-split regex scan, default 100 matches, skips `vendor/node_modules/.git` and a fixed binary-extension list (`file_access_disk.go:148-197, 234-245`) |
| `MkdirAll`, `WriteFile` (whole content), `EditFile([]FileEdit{OldText,NewText})` (first-occurrence search/replace, `file_access_vfs.go:60-77`), `DeleteFile`, `IsReadOnly` | |

Absent from the interface: rename/move, copy, symlink/readlink, truncate, append/partial write, positional read, chmod, fsync, directory removal through the workspace namespace (a dir delete is `EPERM` there, `process_broker_common.go:552-561`; only the in-memory scratch namespaces remove empty dirs, `process_broker_common.go:766-774`), file handles/streams. Read-only-ness is enforced by interface type at injection time (`ReadOnlyFileAccessConsumer`, `file_access.go:96-111`) plus a runtime flag.

Implementations (all `core/versioning/`): `DiskFileAccess` (raw `os.*`, **DISK**, `file_access_disk.go`), `VFSFileAccess` (pipeline overlay, `file_access_vfs.go`; note `Glob`/`Grep` fall back to walking the real disk when the pipeline is not sparse, `file_access_vfs.go:196, 249`), `GlobalDraftFileAccess` (reads green; every write becomes a one-shot pseudo-pipeline `global:<path>:<nanos>` merged through the OT pipe, `global_draft_file_access.go:34-55`, `session_global_draft.go:46-59`), `GlobalVFSFileAccess` (legacy CVS-backed, disk fallbacks, `file_access_global.go`), `BaseFSFileAccess` (read-only over a `vfsBaseFS`), `SessionRoutingFileAccess`/`PipelineRoutingFileAccess` (resolve the live session/pipeline on every call; the pipeline router can silently re-`BeginPipeline` if the VFS vanished, `file_access_pipeline.go:156-184`), `guardedFallbackFileAccess` (writes fail with `ErrNoActiveSessionVFS`, `file_access_guarded.go:9-40`), and `Instrument(...)` which wraps everything in Activity-Fabric spans (`file_access_instrumented.go:23-31`).

How an agent gets one: pods own `ManagedVolume`s (`core/container/pod/volume_manager.go:15-37`); a `VFSVolume` per pipeline binds a pipeline VFS + views on `Mount` — `rebindLocked` first blocks on the session dispatch gate, then calls `BeginPipeline` (`core/container/pod/vfs_volume.go:13-33, 74-118, 264-292`), `DiskVolume` gives architect/guide raw disk (`vfs_volume.go:180-210`), `GlobalVFSVolume` gives global agents green. `VolumeManager.InjectFileAccess` sets the handle on each container agent (`volume_manager.go:243`); `UnmountAll` drains in-flight volume ops through a per-volume `drainTracker` before tearing down (`volume_manager.go:134-192`, `volume_drain.go`).

### 5.2 The LLM-facing tools (skills)

Skills are `skills.Skill` values with a JSON-schema `InputSchema`, description text, domain/keywords/priority, and a `Handler(ctx, json.RawMessage) (any, error)` (`core/skills/skills.go:26-80`); a `Registry` (`skills.go:1052-1181`) exposes them as provider tool definitions (`skills.go:1312`). Prompts reference them by name (e.g. `prompts/librarian/system.md:87`, `prompts/designer/system_guardrails.md:33`).

**`workspace_read`** (`core/versioning/workspace_verbs.go:45-169`) — `op ∈ {read, batch, glob, grep, inspect, summarize, diff, list_changes, prepare_write, prepare_write_batch}`; `view ∈ {disk, global, pipeline}`; `path`, `pipeline_id`, `offset`/`limit` (lines; default limit 1000, `workspace_skills.go:351-386`), `pattern`, `exclude[]`, `include`, `context_lines`, `max_matches` (default 100), `paths[]`, `items[]` (per-path overrides), `budget_bytes` (default 2 MiB, `workspace_batch.go:49-53`), `base_view`/`target_view`, `scope ∈ {pipeline, global}`.
- Return shapes: read → `{path, view, exists, missing, content, total_lines, offset, limit, truncated}`; directory reads degrade to a listing capped at 50 entries (`workspace_skills.go:427-461`, `read_file_result.go:12`); glob/grep → `{matches, count[, truncated]}`; batch → per-path `{status ∈ ok|missing|error|skipped_budget, bytes, content}` (`workspace_batch.go:75-88`); inspect/summarize → multi-layer `WorkspacePathState`/`WorkspaceSummary` (`workspace_views.go:44-93`).
- Error semantics: **missing files are never hard errors** — any `errors.Is(err, os.ErrNotExist)` becomes `missing:true`/empty results (`workspace_skills.go:17-31, 127-137, 176-191, 251-266`); an unavailable view (`ErrVFSNotFound` or message containing "workspace view … unavailable") is soft too (`workspace_skills.go:486-495`); `ErrNoActiveSessionVFS` stays hard; invalid params (`path is required`, `unknown op`) are hard.

**`workspace_write`** (`workspace_verbs.go:178-309`) — `op ∈ {write, edit, delete, mkdir, batch}`, `scope ∈ {pipeline, global}` (required for individual ops), `path`, `content`, `edits[{old_text,new_text}]`, `basis` (object), `operations[]` (batch), `atomic` (batch), `pipeline_id`.
- The **leased write basis**: `prepare_write` returns a `WorkspaceWriteBasis{scope, path, pipeline_id, target_view, prepared_at, lease_expires_at, disk/global/pipeline layer states}` (`workspace_write_skills.go:33-43`); leases last **2 minutes** (`defaultWorkspaceWriteLease`, `workspace_write_skills.go:53`, `1153-1158`). A write validates the basis and returns `WorkspaceWriteStaleError{reason ∈ lease_expired|target_changed|reference_changed}` with the message "rerun prepare_<scope>_write_context" (`workspace_write_skills.go:57-88`); expired/stale bases are transparently refreshed when possible (`workspace_write_skills.go:920-946, 1003-1009`) and a basis can be rebound only for `mkdir` on an ancestor/descendant path (`974-980`). Successful writes return `next_basis` for chaining (`workspace_verbs.go:249`). Writes are refused with "file writes are disabled" when the access is read-only or `WritesEnabledCheck` says no (`workspace_write_skills.go:863-871`).
- **Batch**: ≤ 64 operations (`workspace_batch.go:55-59`), path-dependency DAG layering (mkdir parents first, same-path ops in array order), layers executed sequentially, ops within a layer concurrently on the agent's `GoroutineScope` with a 60 s per-leg timeout (`workspace_batch.go:17-21, 448-551`); `atomic=true` skips remaining layers after the first failure but never cancels in-flight legs.

**Legacy flat skills** still registered for some agents: `read_file`, `write_file`, `edit_file`, `create_directory`, `glob`, `grep` (`core/versioning/file_skills.go:16-366`) — same semantics, no views, no leases.

**Versioning skills** (`core/versioning/skills.go:155-198`): `get_history(file_path?, limit=50)`, `diff_versions(base_version, new_version)` (diffs the *concatenated* delta contents of two WAL entries, `skills.go:354-360` — not a file diff), `rollback_version(target_version)` (major targets rewrite **disk**, minor targets rewrite green, `disk_flusher.go:262-295`), `version_status`.

**Lifecycle verbs** are not agent tools but runtime calls: `BeginPipeline` (pod mount), `handoff_to_ot` → `MergePipelineIntoGreen`, `RollbackPipeline`, `PromoteSessionDraft` → disk flush (`promotion.go:23-32`), `CopyAt(version)` (byte-for-byte materialisation by replaying the **entire** WAL from version zero on every call, `session_copy.go:206-239`), `VFSManager` variant groups (`vfs_manager.go:167-318`; `CleanupStaging` deletes directories on **DISK**, `vfs_manager.go:357-375`).

**Tool execution policy** (`core/toolruntime/policy.go:54-69, 132-209`): every tool has `effect ∈ {read_only, mutating}` and `execution ∈ {local, local_worker, guardian_controlled}`; mutating tools must run on the runtime's single `SerialWorker` (queue 16, one goroutine, `worker.go:24-60`, `runtime.go:1051-1071`) or behind a Guardian grant (`control.go:22-85`). So **all mutating file tools of one agent are serialized**; read-only tools run inline on the caller.

**Persistence classification** (`workspace_mutation_policy.go`): writes made during command execution into `.git`, `__pycache__`, `.pytest_cache`, `.tox`, `node_modules/.cache`, `*.pyc`, `.coverage`, … or anything git-ignored are tagged `ephemeral` (`workspace_mutation_policy.go:28-40, 57-65, 130-190`) and are excluded from merges and disk flushes (`persistentFileModifications`, `67-79`).

### 5.3 What slates needs that sylk does not have

| slates requirement | sylk status | Evidence |
|---|---|---|
| Provision a volume on demand in <50 µs | No volume concept. The closest thing is `acquireWorkspaceImage`, which scans + hashes the whole tree (seconds for a real repo) and is cached only by content signature; a `PipelineVFS` is cheap but is an overlay on a session, not a volume | `workspace_image.go:95-123, 174-195`; `vfs_manager.go:82-99` |
| Attach at a chosen host path (long-lived) | Only per-command FUSE mounts at fixed locations (`/dev/shm/sylk-execfs-*`, `/Volumes/sylk-execfs-<nanos>`, drive letter); mount lives for one `Run`; no API to mount a workspace for an editor or a long-running shell | `process_broker.go:66-88`, `process_broker_linux_common.go:95-104`, `process_broker_darwin.go:165-173`, `process_broker_windows.go:88-100` |
| Detach | `mount.Close()` per run; pod `Unmount` just nils pointers | `process_broker_cgofuse.go:66-77`, `vfs_volume.go:130-144` |
| Archive | None. `ForensicArchiver` is an operator hook; sealed diffs are the only export | `session_vfs.go:236-253` |
| Destroy | `Workspace.Close`/`ChunkStore.Close` unmap the arena; `SessionVFS.Close` tears down; no per-volume destroy with quota reclaim | `workspace.go:191-196`, `session_vfs.go:1060-1171` |
| Bounded (fixed) vs dynamic (auto-resizing) volumes | Only process/session-wide byte budgets (`ChunkStore.memoryLimit`, `memorybudget` scopes, execution governor); no per-volume quota, no resize, no reservation growth policy | `storage.go:132-144`, `core/memorybudget/governor.go:24-27,103-121`, `execution_governor.go:13-23` |
| Many agents concurrently creating/reading/writing/moving | Rename exists only in `Workspace` and the broker's memory namespace; `FileAccess` has no rename; all `Workspace` mutations take one RWMutex | `workspace.go:514-612`, `file_access.go:52-72` |
| Ordinary host tools see a normal path | Only inside a bwrap/sandbox-exec/restricted-token child; the FUSE tree exposes no symlinks/hardlinks/xattrs and forces direct I/O | `process_broker_linux_hanwen.go:94-110`, `process_broker_cgofuse.go:79-196` |
| Snapshot / restore for agents | `Workspace` snapshots are internal; agents only get `rollback_version` on the WAL | `skills.go:179-188` |
| SDKs / MCP | none in these packages (MCP rules are documented only, docs corpus §D) | |

---

## 6. Concurrency model, lock inventory, goroutines, channels, lock-free parts, known races

### 6.1 Lock inventory (one line per lock, what it covers, hot-path cost)

| Component | Lock | Covers | Notes |
|---|---|---|---|
| `Workspace` | `sync.RWMutex mu` (`workspace.go:121`) | snapshots, branches, handles, journal, counters | every mutation and every metadata read of every branch serialize on it; `ReadHandle` only takes RLock to find the handle (`workspace.go:760-768`) |
| `branchGenerations` | `RWMutex` + per-branch `atomic.Uint64` (`write_session.go:33-36`) | fence counters | lock-free reads after first touch |
| `ChunkStore` | 256 × `RWMutex` (`storage.go:93-96`), atomics for bytes/counters | chunk metadata per shard | `Put` slow path takes shard write lock after allocating (`storage.go:169-201`) |
| `chunkArena` | 5 class mutexes + oversize mutex + stats mutex (`chunk_arena.go:113-119, 254, 358`) | slabs/free lists | ≥2 lock acquisitions per allocation |
| `memoryNamespace` | `RWMutex` (`process_broker_common.go:76`) | files/dirs maps | **reads take the write lock** (`process_broker_common.go:716-730`) because reads may inflate |
| `projectedRoot` | `Mutex` (`process_broker_common.go:40`) | handle table | `ReadHandle` copies outside the lock (`process_broker_common.go:273-288`) |
| `executionGovernor` | `Mutex` (`execution_governor.go:31`) | all run budgets | global for all concurrent runs |
| `memorybudget.Governor` | `Mutex` (`governor.go:55-67`) | all reservations | global; touched by every `PipelineVFS` write (`vfs.go:1081-1089`) |
| `hostReadCache` | `Mutex` (`host_read_cache.go:37`) | LRU map+list | global |
| `PipelineVFS` | `RWMutex` (`vfs.go:158`) | all maps | write path does O(files) byte recount under the lock (`vfs.go:1091-1106`) |
| `memorySnapshotFS` / `workspaceImageFS` | `RWMutex` (`basefs.go:60`, `workspace_image.go:416`) | files / pinned snapshot | |
| `workspaceImageRegistry` | `Mutex` (`workspace_image.go:78`) | image refcounts | not held during import (double-import race, §1.2) |
| `ReplicaVFS` | `RWMutex` (`replica_vfs.go:41`) held across ControlWAL fsync on write (`replica_vfs.go:802-823`) | overlay maps, read cache (LRU promotion takes the **write** lock on reads, `replica_vfs.go:464-479`) | parent pointer is `atomic.Pointer` with CAS compression (`replica_vfs.go:62, 263-271`) |
| `SessionVFS` | `mu`, `closeMu`, `mergeCallbackMu` (`session_vfs.go:23-24, 75`), shared `draftMu` (`session_vfs.go:37`) | pipelines map, replica map / close / callbacks / green mutation | `draftMu` serializes MergePipe merges with DiskFlusher flushes |
| `MergePipe` | `Mutex` (`merge_pipe.go:35`) + single goroutine | registrations | |
| `CommitQueue` | `mu` + `subscribeMu` (`commit_queue.go:124-129`) | entries, subscribers | every transition holds `mu` across a ControlWAL fsync |
| `CopyRetention` | `Mutex` (`copy_retention.go:21`) | refs/holders/water line | released before OnRelease callback, then re-acquired in a fixpoint loop (`copy_retention.go:166-188`) |
| `ControlWAL` | `Mutex` + `atomic.Bool closed` (`control_wal.go:36-40`) | file handle | `Compact` holds it for the whole rewrite |
| `VersionedWAL`/`MemoryVersionedWAL` | `Mutex` (`versioned_wal.go:47`, `memory_versioned_wal.go:12`) | index + file I/O | `GetDeltasSince` reads every file under the lock |
| `CommitResolver.flushGate` | `Mutex` (`commit_resolver.go:59-68`) | confirmation state | |
| `DispatchGate` | `Mutex` + `atomic.Bool` (`dispatch_gate.go:55-57`) | broadcast channel swap | |
| `DefaultCVS` | `lockMu`, `subMu`, `pipelineMu`, `statsMu`, `closedMu` (`cvs.go:192-217`) | legacy | |
| `MemoryBlobStore`/`MemoryDAGStore`/`MemoryOperationLog`/`MemoryWAL` | one `RWMutex` each | legacy | copy on every access |
| `VFSManager` | `RWMutex` + per-group `RWMutex` (`vfs_manager.go:53, 65`) | pipeline map | |
| `VolumeManager` | `transitionMu` + `atomic.Bool mounted` (`volume_manager.go:57-63`) | mount transitions | read paths lock-free |
| `toolruntime.State` | `RWMutex` (`state.go:9-12`) | active tools | |

### 6.2 Goroutines and channels

- **MergePipe**: one merge goroutine per session; `mergeCh` buffered 64 (`merge_pipe.go:13, 88`); each request carries a 1-slot `resultCh`; `Stop` closes `stopCh` and drains (`merge_pipe.go:181-219`).
- **CommitResolver**: one loop goroutine per session, woken by `CommitQueue.Subscribe()` events (buffered 64, **non-blocking send, drops on overflow**, `commit_queue.go:617-660`) with a 100 ms ticker fallback; processes ≤16 heads per tick (`commit_resolver.go:161-220`). Flush confirmations run on extra tracked goroutines (`commit_resolver.go:365-379`).
- **DispatchGate**: one watcher goroutine; waiters block on a channel that is closed-and-replaced to broadcast (`dispatch_gate.go:185-193, 229-268`); Stop waits ≤10 s (`dispatch_gate.go:221-226`).
- **DefaultCVS**: one callback dispatcher goroutine with a bounded channel (64), synchronous fallback when full (`cvs.go:205-215, 253-285`).
- **toolruntime**: one `SerialWorker` goroutine per runtime with a 16-deep queue (`worker.go:24-60`); batch legs fan out on the agent `GoroutineScope` (`workspace_batch.go:177-213`).
- **FUSE**: bazil spawns one serve goroutine per mount (`process_broker_linux_bazil.go:66-69`) and the library spawns per-request goroutines; hanwen and cgofuse own their own worker pools. All FUSE handlers call back into the projection under the mutexes above.
- All session goroutines run under a `concurrency.GoroutineScope` ("tracked-goroutine invariant", `session_vfs.go:99-106`); untracked `go` statements are forbidden by project policy.

### 6.3 Lock-free / wait-free parts worth noting

- `ChunkStore` byte budget (`atomic.Int64` add + rollback, `storage.go:132-148`) and per-chunk `atomic.Int32` refs under a shard read lock (`storage.go:157-166`).
- `ReplicaVFS.liveParent` CAS path compression (`replica_vfs.go:263-271`) and `sealed/abandoned` atomics.
- Write-session generation fencing (`write_session.go:142-150`).
- `DispatchGate.Acquire` fast path (`dispatch_gate.go:249-251`), `VolumeManager` read paths (`volume_manager.go:43-45`), `SessionVFS.rejectionEscalation` `atomic.Pointer` (`session_vfs.go:119`).

### 6.4 Known races, bugs, and their root causes

1. **Broker handle read/write race** — `projectedRoot.ReadHandle` copies from `handle.content` after dropping `p.mu` while `WriteHandle`/`TruncateHandle` reallocate `handle.content` under the lock (`process_broker_common.go:273-331`). Concurrent read+write on one FUSE handle is a Go data race (torn/stale read). Root cause: handle content is a growable heap slice rather than an immutable chunk list.
2. **Chunk refcounts never released** (§1.1) — memory of a long session only grows; root cause: `FileBody.Clone` and snapshot replacement don't own chunk references, and there is no snapshot GC. Documented as a known limit: "HAMT structural sharing keeps every snapshot alive forever" (docs corpus §A, SYSTEM_IMPROVEMENTS.md:53-54).
3. **Optimistic close conflicts** — two writers holding handles on the same file: the second `CloseHandle` fails with `ErrHandleConflict` and its data is dropped (`workspace.go:831-833`); there is no merge or retry.
4. **Workspace image double import** — two sessions opening the same root concurrently both scan+import; the loser's 1 GiB-scale import is discarded (`workspace_image.go:109-121`).
5. **Semantic WAL durability gaps** — no fsync on entry files; index capped at 1000 entries so `GetDeltasSince`/`CopyAt` silently see a truncated history once a session exceeds 1000 merges (`versioned_wal.go:17-18, 287-301, 320-326`).
6. **Subscriber event drops** — `CommitQueue.emit` drops on a full channel; correctness relies on the resolver's ticker and on `DispatchGate` re-checking depth; a dropped `committed` event delays dispatch by up to one tick (`commit_queue.go:644-660`, `commit_resolver.go:127-129`).
7. **DispatchGate over-commit** — signal-only; N concurrent callers can all pass with one free slot (`dispatch_gate.go:33-39`).
8. **`CopyAt` cost** — replays the whole WAL from zero and materialises every file ever touched, on every call (`session_copy.go:86-90, 218-239`); the comment defers "periodic snapshot checkpoints" to a later stage.
9. **Not-found wrapping bug (fixed)** — `os.IsNotExist` does not unwrap `%w`, so `ErrFileNotFound` became a hard tool error that cascaded into `AbortToolLoop`; fixed with `errors.Is(err, os.ErrNotExist)` (`workspace_skills.go:17-31`; docs corpus §D, `docs/bugs/2026-06-14-workspace-read-wrapped-not-found-hard-error.md`).
10. **Slow globs starve liveness** — `workspace_read` globs of 8–10 s each (disk walks through `VFSFileAccess.vfsGlob` → `diskGlob`) stalled the claim watchdog because liveness only fires at tool completion (docs corpus §E, `docs/bugs/2026-06-15-carry-forward-wedge-three-causes.md:33-38`).
11. **Unmount-before-pause race** in the pod `VolumeManager` (docs corpus §E, SYSTEM_IMPROVEMENTS.md:132-134) — mitigated by the drain barrier (`volume_drain.go`, `volume_manager.go:134-192`), but a volume op that does not call `BeginVolumeOp` is invisible to the drain (`volume_manager.go:47-52`).
12. **Commit-WAL segment accumulation** (knowledge-graph WAL, same design family): a segment minted per boot, `Checkpoint()` with zero callers, GC only after crashes, and a latent bug where GC could remove the open segment — 5,499 segments averaging 78 bytes (docs corpus §C, `docs/bugs/commit-wal-segment-accumulation.md:4-30`).
13. **ReplicaVFS recursion** — `Stat` and `collectListDir` recurse through the parent chain (`replica_vfs.go:659-664, 739-742`), so stack depth is O(chain) although `Read` was deliberately made iterative (`replica_vfs.go:572-592`).
14. **Reads serialize inside a sandbox** — `memoryNamespace.ReadFile` takes the write lock (`process_broker_common.go:716-730`), so parallel test workers reading `/tmp` or `/cache` contend on one mutex.
15. **FUSE metadata staleness window** — hanwen entry/attr caches of 100 ms (`process_broker_linux_hanwen.go:156-165`) can show phantom or stale entries while the projection mutates underneath; bazil uses no attr caching but direct I/O for data.

---

## 7. Every measured or claimed performance number (with conditions)

Legend: **M** = measured somewhere in the repo (test output, bug report, benchmark log); **C** = claimed in a comment or design doc without a recorded measurement; **K** = a constant the code simply chose (a "magic number" in slates' terms). Docs-corpus citations refer to `survey-sylk-docs-corpus.md` (the helper's verbatim report) and through it to the sylk docs.

### 7.1 Benchmarks that exist, and what they recorded

- The only VFS benchmarks in the tree are `BenchmarkChunkStore_PutUniqueChunks` (64 KiB unique puts), `BenchmarkChunkStore_PutDedupedChunks` (~64 KiB identical puts) and `BenchmarkChunkStore_CopyInto` (64 KiB copy) (`core/purevfs/chunk_arena_test.go:425-467`). **No results are recorded anywhere** (grep for `ns/op`, `µs/op`, `MB/s` across `docs/` and tests finds none for purevfs/versioning). `core/versioning`, `core/toolruntime` and `core/filesystem` have **zero** `Benchmark*` functions.
- Race/stress tests exist but assert only correctness: 32 goroutines × 200 allocations (`chunk_arena_test.go:148-183`); 16 writers × 200 put/copy/release (`chunk_arena_test.go:299-338`); 200 create/close cycles to pin the mmap leak fix (`chunk_arena_test.go:362-382`).
- Measured numbers that do exist in the repo are for **other** subsystems (docs corpus §F, `survey-sylk-docs-corpus.md:105`): DB `Read @ 0.24 ns/op`, `AppendParallel @ 107 µs/op` ("lock-free COW"), `NextID @ 3.5 ns/op`, marshal/unmarshal 15–274 ns/op (`docs/DB_TODO.md:59-86`); graph `FullCommitWorkflow 2.4 ms/op`, `CommitFileRoundTrip 3.4 ms/op` (`docs/GRAPH_TODO.md:945`); `Remove-Birth ~10 ms/operation` (`docs/GRAPH_OPTIMIZATIONS.md:365`). None of these are the VFS.

### 7.2 Measured latencies from incidents (M)

- `workspace_read` glob calls taking **8–10 s each** (disk walks through `vfsGlob → diskGlob`, `file_access_vfs.go:196`) — long enough to trip the claim-inactivity watchdog (docs corpus §E/§F, `docs/bugs/2026-06-15-carry-forward-wedge-three-causes.md:33-38`).
- Commit WAL (knowledge graph): **5,499 segments, average 78 bytes, ~21 MB** after segment-per-boot minting (docs corpus §C, `docs/bugs/commit-wal-segment-accumulation.md:4`).
- Legacy session WAL: checkpoint every 5 s, "maximum data loss: 5 seconds of work" (docs corpus §C, ARCHITECTURE.md:6821-6852) — a design number, not a benchmark.

### 7.3 Claims in code comments and design docs (C)

| Claim | Where | Condition / caveat |
|---|---|---|
| DEFLATE level 1: "~2-3x compression … at ~500 MB/s on commodity CPUs" | `memory_compression.go:146-149` | no benchmark; Go `compress/flate` BestSpeed on "pytest cache, npm logs, source files" |
| "DEFLATE on 300 bytes of a single byte hits ~6x" | `memory_compression_test.go:48-49` | test only asserts `compressed == true` |
| Snapshot memory O(N + S·log N) vs O(N·S) for the old clone-per-mutation design; "hundreds of MiB of duplicated inode tables" before | `workspace.go:57-69` | analytic; the HAMT is `benbjohnson/immutable` |
| "stat syscall is ~3 orders of magnitude cheaper than a multi-megabyte read" (host read cache) | `host_read_cache.go:19-24` | justification for stat-on-every-lookup |
| 128 MiB "enough to hold a typical Python runtime (~30 MiB across interpreter + stdlib) plus a node_modules hotset" | `host_read_cache.go:52-55` | sizing rationale |
| 16 slots per slab: "a 64 KiB class slab is 1 MiB, which absorbs the default chunking pass for a multi-megabyte file" | `chunk_arena.go:61-67` | sizing rationale |
| hanwen: "passthrough mode for near-native read perf"; bazil "~16 months stale" | `process_broker_linux_hanwen.go:13-21` | passthrough is not actually enabled (direct IO is) |
| hanwen attr/entry timeout 100 ms "short enough to feel fresh … while amortizing per-attr lookup cost" | `process_broker_linux_hanwen.go:156-161` | |
| Dispatch cap = GOMAXPROCS × 2 audit replicas: "read amplification is O(depth)" | `dispatch_gate.go:72-89` | derived, not measured |
| Read-cache capacity = (touched paths across chain) × 8 "single-digit dependency neighborhood" | `replica_vfs.go:431-457` | derived |
| `CopyAt` "O(disk baseline + total deltas to version) per call"; "codebases in the typical range … a few MB" per materialization | `session_copy.go:19-22, 86-90` | analytic |
| Batch read budget 2 MiB "≈ one LLM turn's context (1 MiB ≈ 250k tokens at 4 chars/token)" | `workspace_batch.go:49-53` | |
| Docs (corpus §F, `survey-sylk-docs-corpus.md:98-105`): BLAKE3 "~5× faster than SHA-256"; CPython build peak scratch "~2GB"; cache tiers Hot 25 % / Warm 35 % of system memory; TinyLFU 4-bit sketch; zstd 64 KB dict "5-8× with a Python-trained dict vs ~3×"; pytest versions "share ~95% of their blobs"; cost "O(unique_bytes), not O(pipelines × packages)"; claims scope check "≤ 50 µs per write (cached)"; DB p99 targets GetNode <500 ns / FindNode <1 µs / AddNode <10 µs / Checkout <1 µs; PERFORMANCE.md WAL 64 MB segments, 100 ms sync interval, 32 MB hot cache | design targets for the Tool-VFS substrate / DB, not measurements of the surveyed code |

### 7.4 Constants that shape performance (K) — the successor must derive these, not copy them

| Constant | Value | Where |
|---|---|---|
| Chunk size | 64 KiB | `storage.go:449`, `workspace_image.go:26` |
| Arena size classes / slots per slab | 4 K, 16 K, 64 K, 256 K, 1 MiB / 16 | `chunk_arena.go:53-59, 67` |
| ChunkStore shards | 256 (hash[0]) | `storage.go:80-81` |
| Chunk hash | SHA-256 over every chunk on every `Put` | `storage.go:29-31, 154` |
| Workspace image cap / ChunkStore limit | 1 GiB (`SYLK_WORKSPACE_IMAGE_MAX_BYTES`) | `workspace_image.go:25-27` |
| Global memory budget | total 2 GiB; workspace-image 1 GiB; overlay 512 MiB; execution 512 MiB | `core/memorybudget/governor.go:24-27` |
| Execution governor | run 256 MiB; reserve 16 MiB; stdout/stderr 8 MiB each | `execution_governor.go:19-22` |
| Host read cache | 128 MiB LRU | `host_read_cache.go:56` |
| Windows job memory limit | = run limit | `process_broker_windows.go:109, 228-231` |
| FUSE: hanwen attr/entry TTL; bazil close wait; cgofuse mount-ready timeout | 100 ms; 250 ms; 2 s | `_hanwen.go:162-165`; `_bazil.go:96`; `process_broker_cgofuse.go:55` |
| Commit resolver poll / heads per tick | 100 ms / 16 | `commit_resolver.go:93-96, 200` |
| Merge channel / queue subscriber buffer / CVS dispatcher buffer | 64 / 64 / 64 | `merge_pipe.go:13`, `commit_queue.go:626`, `cvs.go:176-178` |
| Dispatch gate cap / stop wait | GOMAXPROCS × 2 / 10 s | `dispatch_gate.go:70, 83-89, 224` |
| Semantic WAL retained majors / index cap | 10 / 1000 | `versioned_wal.go:17-18` |
| Legacy MemoryWAL max entries | 10 000 | `wal.go:75` |
| Session background-scope shutdown | 500 ms grace / 5 s hard (close); 100 ms / 2 s (start failure) | `session_vfs.go:1165, 324` |
| Replica read-cache fanout | 8 per touched path | `replica_vfs.go:438` |
| Write lease | 2 min | `workspace_write_skills.go:53` |
| Batch read budget / write ops cap / leg timeout | 2 MiB / 64 / 60 s | `workspace_batch.go:53, 59, 21` |
| Read defaults: lines / grep matches / dir entries | 1000 / 100 / 50 | `workspace_skills.go:354`, `file_access_vfs.go:124-126`, `read_file_result.go:12` |
| Tool serial worker queue | 16 | `worker.go:36-37` |
| FilesystemManager max file | 100 MiB | `core/filesystem/manager.go:61` |
| Control/semantic WAL entry header | 25 bytes + CRC32 | `control_entry.go:176-180`, `wal_entry.go:67` |
| ExecutionPlan sandbox defaults (legacy docs) | CPU 80 %, 1024 MB, 256 files, 32 procs, 30 s | docs corpus §F (ARCHITECTURE.md:20504-20508) |

### 7.5 Analytic cost observations for the implementer (not sylk claims)

- Provisioning today: `acquireWorkspaceImage` = full tree walk + read + SHA-256 of every byte (`workspace_image.go:174-349`), then a per-file 64 KiB chunk loop with a SHA-256 per chunk. This is O(repo bytes) with cryptographic hashing — the opposite of a 50 µs provision. slates must provision by pointer-copying an already-resident snapshot.
- Per-write cost in `PipelineVFS`: O(number of overlay files) byte recount + governor mutex + several full-content clones (`vfs.go:333-355, 1049-1106`). Per-write cost in `Workspace`: path-depth node clones + HAMT insert + SHA-256 of the whole written buffer + arena mutex ×2 (`workspace.go:433-461`, `storage.go:150-202`).
- SHA-256 of a 64 KiB chunk at typical software throughputs (≈1–2 GB/s) is on the order of tens of microseconds by itself; hashing on the write path therefore cannot coexist with a 50 µs budget for any operation that writes real data. Hash lazily (at commit/dedup time) or use a faster hash (BLAKE3/xxh3 as the Tool-VFS design already proposes, docs corpus §F).
- FUSE path: every `open` loads the whole file into a fresh heap buffer and every `release` writes it back through `WriteFile` → `PipelineVFS.Write` → clone + recount (`process_broker_common.go:355-407, 377-386`). A 10 MiB file opened for a 1-byte read costs a 10 MiB copy and a 10 MiB budget reservation.

---

## 8. Verdicts for the Rust successor

### 8.1 KEEP (carry these ideas forward, and why)

1. **Immutable per-mutation snapshots with structural sharing** (`workspace.go:57-79`): path-copied directory tree + persistent inode map gives O(depth + log N) mutations, O(1) branch forks (`workspace.go:208-233`) and trivially safe concurrent readers on old snapshots. This is exactly the substrate a CoW volume needs; in Rust use an arena-allocated persistent trie/HAMT with epoch reclamation instead of GC.
2. **Content-addressed chunks with a hash-prefix-sharded index and atomic refcounts taken under a shared lock** (`storage.go:75-96, 157-166`): dedup across volumes and versions falls out for free; the "shard count = cardinality of the index byte" reasoning (`storage.go:75-79`) is the right way to avoid magic numbers.
3. **Off-heap size-classed slab arena with slot reuse and lazily-populated anonymous mappings** (`chunk_arena.go`, `chunk_arena_unix.go:11-34`): bulk bytes should never be individually heap-allocated; slabs give predictable allocation cost. Keep the idea, add page-size derivation and `madvise` reclaim.
4. **Extent lists with blob/zero kinds, sub-ranging and adjacent-extent merging** (`storage.go:369-400, 659-695, 776-817`): supports sparse files and partial rewrites without touching untouched chunks — but index them better (see IMPROVE 3).
5. **Generation-fenced write sessions** (`write_session.go:73-209`): a one-atomic-load fence per write is the cheapest possible multi-agent ownership handoff; `Seal` returning a pinned snapshot + next generation is a clean receipt.
6. **Three-state overlay resolution (hit / shadowed / miss) and union-find compression of abandoned layers** (`replica_vfs.go:554-592, 263-271`): delete shadows must never fall through to a lower layer, and rejected layers must physically leave the chain. Keep both invariants.
7. **The WAL *shape*** — fixed 25-byte header, CRC per entry, truncate-at-first-corruption, seq monotonic across compaction with a high-water marker, majority-rule compaction (`control_wal.go:14-34, 84-133, 250-362`) — as an in-memory operation log for replay/undo/export. slates must not write it to disk, but the log discipline is right.
8. **A single serializer per mutable target** (`merge_pipe.go:31-46`) and a **state-machine commit queue with write-ahead transitions** (`commit_queue.go:10-40, 103-122`): one owner, explicit states, idempotent replay. Generalize to "one owner task per volume".
9. **Explicit, named retention holders with a water line and fixpoint GC** (`copy_retention.go:61-189`): refcounts keyed by holder ID are debuggable and idempotent; the fixpoint cascade avoids one-layer-per-advance unwinding.
10. **Host-derived limits** (`DeriveDispatchMaxInFlight`, `dispatch_gate.go:72-89`; cache capacity from the audit's own path count, `replica_vfs.go:446-457`; shard count from hash width) and **hard failure instead of spill** under memory pressure (`memory_compression.go:24-27`, `memory_compression_test.go:232-254`). Both match slates' design rules.
11. **Hierarchical memory governance with reserve/resize/release** (`core/memorybudget/governor.go:103-121`, `execution_governor.go:117-202`): every byte held for an agent is attributed to a scope and can be refused. Rebuild it lock-free.
12. **Agent-facing ergonomics**: soft `missing:true` results instead of hard errors (`workspace_skills.go:17-31, 388-405`), directory-read degradation (`427-461`), structured `truncated`/`unavailable` flags, leased write bases with explicit staleness reasons (`workspace_write_skills.go:57-88`), batch ops with dependency layering and per-op results (`workspace_batch.go:23-47`). These reduce wasted LLM turns and belong in slates' SDK/MCP surface.
13. **Mutation-origin tagging** (`workspace_mutation_policy.go:57-65`): distinguishing tool-generated ephemera (`__pycache__`, caches) from durable authored content lets a volume discard noise at snapshot/archive time.
14. **Per-agent serialization of mutating tools** (`policy.go:165-179`, `worker.go`) and **tracked task scopes** (`session_vfs.go:99-106`): predictable ordering per writer; no orphaned tasks.

### 8.2 AVOID (design mistakes or limits, and why)

1. **Whole-file `[]byte` as the unit of storage in overlays** (`vfs.go:164-172`, `replica_vfs.go:76-99`, `process_broker_common.go:82-93`, `FileModification.NewContent/OldContent`, `WALFileDelta` old+new): every layer copies full contents, often 3–5× per write; reads clone defensively (`cloneBytes` everywhere). A CoW VFS must reference chunks, never copy bytes across layers.
2. **One chunk per write and no re-chunking** (`storage.go:583-603, 753-770`): destroys dedup between versions, creates one `mmap`/`munmap` pair per >1 MiB write, and makes small edits of big files cost the whole file.
3. **Refcounts that are never decremented and snapshots/journals that are never reclaimed** (§1.1, `workspace.go:947, 963`): memory grows monotonically for the life of a session; correctness of `Release` was proven only in tests (`chunk_arena_test.go:222-252`).
4. **Coarse locks on hot paths**: one RWMutex per `Workspace`; global mutexes in the memory governor, execution governor and host read cache; reads that take write locks (`process_broker_common.go:716-730`, `replica_vfs.go:464-479`); mutex held across fsync (`replica_vfs.go:794-823`, `commit_queue.go:219-248`). This is the throughput ceiling sylk's own docs admit (docs corpus §E, SYSTEM_IMPROVEMENTS.md:118-119).
5. **Disk fall-through baked into the types**: `PipelineVFS`, every `FileAccess`, the WALs, the flusher and the rollback skill all reach `os.*` (§4.2). "Strict-RAM" is a configuration, not a guarantee. slates must make host I/O impossible inside the volume core by construction (no `std::fs` in that crate).
6. **Per-command FUSE mounts at fixed tmpfs/`/Volumes`/drive-letter locations** with no long-lived attach, no crash cleanup, three FUSE libraries, path-hash inode numbers (rename changes the inode; hard links impossible; `process_broker_linux_common.go:114-118`), forced direct I/O (no shared `mmap`, no page cache), a truncated op set (no symlink/xattr/fsync/statfs), and argv/env/cwd string rewriting on macOS/Windows (`process_broker.go:267-346`). Tools like `git`, `cargo`, editors and language servers expect stable inodes, `mmap`, symlinks and `fsync` to succeed.
7. **Byte-level OT on whole-file diffs as the merge engine** (`merge_pipe_precise.go:26-74, 97-140`) with a default no-op conflict resolver (`merge_pipe.go:76-79`) and a blocked queue on rejection (`commit_resolver.go:253-255`): expensive (diff-match-patch per file per merge, re-diffing every accumulated delta on the same path) and semantically weak for code.
8. **Whole-overlay disk flush and "union" confirmations** (`disk_flusher.go:60-111`, `commit_resolver.go:381-385, 463-470`): a per-descriptor commit that cannot be isolated defeats the FIFO queue's purpose.
9. **O(repo) provisioning**: full tree walk + SHA-256 signature + per-chunk SHA-256 (`workspace_image.go:174-402`); `CopyAt` replays the whole WAL per call (`session_copy.go:218-239`); `PipelineVFS` recounts all overlay bytes on every write (`vfs.go:1091-1106`).
10. **Flat absolute-path maps as the directory index** (`memoryNamespace`, `PipelineVFS`, `ReplicaVFS`): `ListDir`/`hasChildren`/rename scan every entry by string prefix (`process_broker_common.go:666-688, 811-823, 903-922`); parent/child relationships are recomputed per call.
11. **Legacy layers still constructed per session** (CVS shim, blob/DAG stores, `MemoryWAL`, `memorySnapshotFS`; `session_vfs.go:294-306`) — dead weight and a second source of truth.
12. **Env-var magic budgets** (`SYLK_EXECUTION_*`, `SYLK_WORKSPACE_IMAGE_MAX_BYTES`, 512 MiB/256 MiB/16 MiB/8 MiB/128 MiB, `execution_governor.go:13-23`, `host_read_cache.go:56`) instead of measuring the host; and **timeouts as coordination** (250 ms unmount wait, 100 ms poll, 2 s mount-ready, 10 s stop) plus **dropped events by design** (`commit_queue.go:644-660`).
13. **Runtime-specific crutches**: finalizers as leak safety nets (`chunk_arena.go:126-134`), `context.Background()` in every cgofuse handler (no cancellation), heap-backed arena on Windows (`chunk_arena_windows.go`), Go-heap fallback when `mmap` fails (`chunk_arena_unix.go:27-32`) that silently changes memory accounting.
14. **Unsafe slot lifetime**: after `Close`, dangling slices into unmapped slabs crash the process (`chunk_arena.go:213-219`); ownership of chunk bytes is by convention, not by type.

### 8.3 IMPROVE (what to redesign, and in which direction)

1. **Volume provisioning as pointer work**: a volume is `{root snapshot handle, quota, generation, owner}` taken from a pre-sized pool; forking a snapshot is copying one pointer and bumping an epoch (sylk's `CreateBranch`, `workspace.go:208-225`, is already O(1) — make the *whole* provision path that cheap: no directory scan, no hashing, no map allocation at provision time). Base images are imported once per host, shared by all volumes.
2. **Chunk-granular CoW with re-chunking on write**: fixed-size (page-multiple, measured against the host's page size and the workload's median file size) or content-defined chunks; a partial write rewrites only the touched chunks; refcounts are released when a snapshot is reclaimed. Hash lazily (on dedup/commit) with a fast hash; keep a per-volume "unhashed dirty" state so the write path never pays SHA-256.
3. **Index structures**: replace sorted extent slices rebuilt per write with a persistent B-tree/segment tree keyed by logical offset; replace `map[string]*treeNode` children with a persistent ordered map so `readdir` is O(entries) and rename is O(1) pointer moves with inode numbers preserved.
4. **Reclamation**: epoch-based (RCU-style) snapshot reclamation — readers pin an epoch, writers publish new roots by atomic pointer swap, reclaim when the oldest pinned epoch passes; journal retention bounded by a policy derived from memory pressure, not "forever".
5. **Shared-nothing concurrency**: one owner task per volume consuming a bounded command queue (sylk's `MergePipe` generalized, `merge_pipe.go:31-46`), readers served from immutable snapshots without locks, cross-volume operations as messages; per-shard chunk indices owned by shards, not guarded by mutexes; memory accounting via per-core counters folded into hierarchical budgets.
6. **Host projection built for long-lived attach**: one mount per host namespace with volumes attached/detached as subtrees at chosen paths; stable inode numbers from inode IDs; full POSIX op set (rename preserving inode, hard/sym links, xattrs optional, `fsync` as a no-op success, `statfs` reporting the volume quota); kernel cache kept coherent with explicit invalidation (`notify_inval_entry/inode`) instead of direct I/O so `mmap`, `exec` and page-cache reads work; crash-safe cleanup (mount registry + lazy unmount on restart). On macOS evaluate FSKit (no kext, Apple-supported) versus an in-process NFSv4 loopback like FUSE-T; on Windows WinFsp or ProjFS with stable file IDs. Never rewrite argv/env — the path must be the same inside and outside.
7. **Merging and commits**: commits are snapshot pointer swaps under the volume owner; merges are explicit three-way merges at file/chunk granularity with conflicts surfaced to the caller (not silently resolved by a no-op resolver); rejected work forks rather than blocking a FIFO.
8. **Durability without disk**: keep sylk's WAL discipline as an in-memory, bounded operation log per volume for undo/replay/observability; expose an explicit `archive` verb that streams a sealed snapshot (chunks + tree) to the *client*, and a `restore` that rebuilds from a stream — slates itself never opens a file.
9. **Quotas**: bounded volumes reserve their full budget at provision (guaranteed, fail-fast); dynamic volumes grow in measured slab increments against the host budget with backpressure and cold-chunk compression (measure LZ4/zstd on real chunk samples and pick by data, unlike sylk's unmeasured DEFLATE claim, `memory_compression.go:146-149`).
10. **API**: explicit lifecycle verbs (`provision`, `attach(path)`, `detach`, `snapshot`, `fork`, `merge`, `archive`, `restore`, `resize`, `destroy`, `list`, `stat`) plus a POSIX-complete file verb set (rename/copy/symlink/truncate/ranged read/write), with sylk's soft-not-found results, structured truncation, generation-fenced write leases (`write_session.go`) and batch ops with dependency layering (`workspace_batch.go`).
11. **No magic numbers**: derive page size, cache-line size, core count, chunk size (from measured file-size distribution), shard count (from hash width and core count), slab size (from measured allocation bursts), lease and timeout values (from measured operation latencies); ship the benchmarks sylk lacks (§7.1) and record baselines in the repo.
12. **Observability**: keep sylk's chokepoint instrumentation idea (`file_access_instrumented.go:10-31`, `execution_broker_instrumented.go:10-27`) but make it zero-cost when disabled and attribute every byte and every lock-free operation to a volume and an agent.
