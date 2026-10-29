# Design: the catalog, readable names for runtime and model descriptors

**Status: proposal. Nothing here is implemented.** The descriptors themselves are
[storage.md](../storage.md#model_descriptor--runtime_descriptor); how a plan
becomes jobs is [plan-ingestion.md](../plan-ingestion.md).

## The problem

- **A descriptor is exact, but a person cannot read it.** A plan, a job and a
  result row each carry the full runtime and model descriptors, for example
  `{"flavor":"macos-arm64","repository_url":"github.com/ggml-org/llama.cpp","repository_version":"b11669","source":"github_release","type":"llamacpp_cli_stock_tools"}`.
  The console shows this JSON in every job row.
- **Upstream versions do not tell a person what they are.** llama.cpp publishes a
  `b<N>` build several times a day, and forks use git revisions such as
  `23dff81c4`. A stable release such as `v0.7.0` ships no binaries. Its only
  asset, `nightly-tag.txt`, names the `b<N>` build at the same commit.
- **One version is many descriptors.** A llama.cpp build is one descriptor per
  flavor, and a release has about 30 flavors.
- **Nobody can ask what is available.** No route lists the runtimes or models
  that a user can name, or tells which of them run on a given device.

## Principle

**A catalog name is a label on descriptors. It never replaces them.** Plans, jobs
and result rows carry the full descriptor and its digest, as they do today. The
catalog adds a readable name on top.

| Property | Because |
|---|---|
| A new member never changes the member that a name selects for a device | a new member can only go at the end of the precedence, and a name is never used twice |
| A result row shows which build ran | a row shows `name::flavor`, never the bare name |
| The catalog only names builds | the catalog stores no claim about success; run evidence comes from result rows and never selects an entry |
| Results from before the catalog can get names | the console looks up names by descriptor digest, exactly or through a verified alias |
| A stored plan never changes meaning | ingestion replaces each name with its descriptor before it stores the plan |
| A device is offered only what it can run | offer rules deny by default |
| A name means the same thing to every reader of its scope | each scope has one namespace, and a reader outside an organization sees the qualified name |
| Catalog reads follow their scope | a global name names only public artifacts. Only the members of an organization and the holders of `edit_any_catalog` read an org entry |
| A plan that gives a name makes the same jobs as a plan that gives the descriptor | the catalog adds no eligibility clause and changes no matching rule |

**The catalog does not change matching or eligibility.** These stay exactly as
[plan-ingestion.md](../plan-ingestion.md) describes them:

- the eligibility clauses of a job: `requires`, `any_of`, `clients`,
  `excludes`, `not_all_of` and `exclude_clients`;
- capability matching, which decides which device can claim a job;
- the planner's capability rules, which add `any_of` groups at expansion.

The catalog's own rules, the offer rules ([below](#offer-rules-what-a-device-is-offered)),
decide only two things: which members the catalog offers to a device, and which
member a name selects. A job never reads an offer rule.

## Terms

- A **catalog name** is a short string, such as `llamacpp-v0.7.0`, that stands
  for one or more descriptors.
- An **entry** is one catalog name and its members.
- A **member** is one descriptor of an entry.
- The **kind** of an entry is `stable`, `daily` or `hand-made`. The name pattern
  shows the kind.
- An **offer rule** tells which devices the catalog offers a member to. It is
  not a job eligibility clause.
- A **member key** identifies one member of an entry. For a runtime, the member
  key is the flavor. For a model, the member key is a format and a
  quantization ([below](#models)).
- The **scope** of an entry is `global` or one organization
  ([below](#scopes)).
- A **global entry** has the global scope. Every account can read it.
- An **org entry** belongs to one organization. Only the members of that
  organization read it. "Private catalog" is a different thing: it names the
  benchmarks that an organization keeps to itself.
- A **qualified name** gives the scope in front of the name, for example
  `global/my-fork` or `acme/my-fork`. The part before `/` is `global` or an
  organization slug.

## Entries and members

An entry holds:

- the scope;
- the catalog name;
- an ordered list of members, the **precedence**;
- the kind;
- the lifecycle state ([below](#lifecycle));
- the release time;
- the time of creation.

Each member also records the time that it was added.

**The release time is when upstream published the build that the name stands
for.** It controls the order of the lists and automatic expiry. The time of
creation is only a record of when the catalog got the entry. The two times
differ most for an entry from a backfill: its release can be months old on the
day that the catalog creates it.

| Kind | Release time |
|---|---|
| stable | the publication time of the `vX.Y.Z` release |
| daily | the publication time of the `b<N>` build; its UTC date is the date in the name |
| hand-made | the time that the operator gives in the `create` file; without one, the time of creation |

The release time is fixed when the entry is created.

**All members of one entry share one runtime type and one upstream version.**
They differ only in flavor or build target. Docker vLLM and uv vLLM are
different engines that give different numbers, so they get different entries.

**An entry can get more members later, but only at the end.** Upstream can
attach a flavor to a release after the release appears, and the flavor table
can learn an asset that was there from the start. So an entry can get new
members after it is created, with three limits:

- **The same build.** A new member differs from the other members only in its
  flavor. For llama.cpp, it has the same repository and the same `b<N>` tag. An
  entry never mixes builds.
- **The end of the precedence.** A new member goes after every member that the
  entry has. A device that resolved the name before resolves it to the same
  member after.
- **No other change.** Nothing removes, changes or reorders a member.

A different build is a new entry under a new name. An entry with one member is
valid. That is the usual shape of a hand-made entry for a custom build.

**`name::flavor` selects one member explicitly**, for example
`llamacpp-v0.7.0::linux-x64-vulkan`. The part after `::` is the flavor as a
descriptor spells it, not the upstream asset name. A bare name selects a member
by precedence
([below](#which-member-a-name-selects)).

**Names follow one grammar.** A name matches `[A-Za-z0-9]( ?[A-Za-z0-9._-])*`.
A single space can separate two other characters, for example
`LFM2.5 Audio 1.5B`. A name has at most 200 characters.

The catalog shows a name in the case that its creator wrote. Uniqueness and
lookup ignore case, so two names cannot differ only in case. A name contains
no `:`, so `::` is always
the member separator. A name contains no `/`, so `/` always separates a scope
from a name.

**Runtimes and models share the namespace of their scope.** The console shows
them on separate tabs. One namespace means that a name never needs its type to
be clear.

**The catalog creates its descriptors in minimal form.** Each field that holds
its default value is absent. The catalog does not normalize a descriptor that
an author writes: ingestion stores that descriptor as written.

### Kinds and name patterns

| Kind | Pattern | Created by |
|---|---|---|
| stable | `llamacpp-v0.7.0` | the importer |
| daily | `llamacpp-daily-2026-10-19` | the importer |
| hand-made | a global name outside a reserved pattern, or any org name | an operator with the host CLI, or a member of the organization |

**Each importer reserves only its own patterns.** The llama.cpp importer
reserves `llamacpp-v*` and `llamacpp-daily-*` in the global scope. An operator
can still create `llamacpp-example-fork-2026-10` for a fork build. A reserved
pattern stops a global hand-made name from colliding with a name that an
importer creates later. An importer writes only global entries, so a reserved
pattern does not apply to an org name ([below](#scopes)).

### One descriptor, several names

A stable release and the daily build of the same commit have the same
descriptors. So a descriptor can have several names. The console shows one of
the names that the reader can read, in this order:

1. an org entry of the reader's organization;
2. a global hand-made entry;
3. a stable entry;
4. a daily entry.

A hand-made name comes first because a person made it on purpose. An org name
comes before a global hand-made name because the organization chose it for its
own work. The console shows the other names in a tooltip.

## Scopes

Each entry has one scope: `global`, or one organization.

| | Global entry | Org entry |
|---|---|---|
| Kinds | stable, daily and hand-made | hand-made only |
| Created by | an importer, or an operator with the host CLI | a member that holds `edit_catalog`, a holder of `edit_any_catalog`, or an operator with the host CLI |
| Read by | every active account | the members of the organization and holders of `edit_any_catalog` |
| Artifacts | public artifacts only | also the organization's vault resources, such as the vault copy of a private or gated model |
| Expiry | by kind ([below](#lifecycle)) | only by hand |

**Each scope has its own namespace.** Two organizations can each have an entry
named `my-fork`, and the two entries are not related. Uniqueness and lookup
ignore case inside one scope.

**An org name can shadow a global name.** An organization can create an entry
with the name of a global entry, also the name of an importer entry such as
`llamacpp-v0.7.0`. Inside the organization, the bare name means the org entry.
Outside the organization, the bare name means the global entry. A global entry
that an operator creates later does not change this: inside the organization,
the org entry still has the name.

**A qualified name is the same for every reader.**

- `global/<name>` always means the global entry. A member of an organization
  writes this form to select a global entry that an org entry shadows.
- `<org-slug>/<name>` always means the entry of that organization, for example
  `acme/my-fork`.

`global` is a reserved organization slug, so no organization can have it. An
organization slug can change. The catalog stores the organization id, and
reads the slug when it shows a name. So a slug change only changes the
qualified names that the console shows.

**The members of an organization read its entries on membership alone,** as
they read its private benchmarks. A holder of `edit_any_catalog` reads the
entries of every organization, also when it belongs to an organization. It sees
the entries of its own organization with bare names, and these names can shadow
global names. It sees the entries of other organizations with qualified names.
For every other reader, an entry outside its organization does not exist: a
lookup gives the same answer as for an unknown name.

**An org entry can name the organization's private artifacts.** A global name
names only public artifacts, because every account reads it. Only the members
of the organization and the holders of `edit_any_catalog` read an org entry, so
it can also name:

- a vault resource that the organization owns;
- a private or gated Hugging Face model, through its vault copy. Ingestion
  makes this copy when a plan gives the model with an access token
  ([plan-ingestion.md](../plan-ingestion.md#the-import-of-a-gated-model)).

The digest of a vault member does not include the vault `custom_label`, as the
digest in the warehouse does not.

**An org entry is always hand-made.** No importer writes an org entry, and
automatic expiry never hides one. A member of the organization pins, hides and
tombstones it ([below](#who-changes-the-catalog)).

**A tombstone reserves a name in its own scope.** A tombstoned org name cannot
be used again in that organization. Other organizations and the global scope
can still use the name.

**`orgs delete` refuses an organization that has a live entry.** A live entry
is an entry that is not tombstoned. This is the same rule as for private
benchmarks. When `orgs delete` deletes an organization, it also deletes the
tombstoned entries of the organization.

**The creator of an org entry is an account.** The entry records the creator as
`account:<user_id>`. A global entry records `operator` or the importer that
created it.

## Offer rules: what a device is offered

**Offer rules live in the server configuration.** Each rule maps a runtime type
and a flavor to clauses over capability flags. The clauses use the plan
vocabulary, `requires` and `any_of`, and the existing matching code evaluates
them. They are a separate set of rules: they do not change the planner's
capability rules, and they never reach a job. The configuration is kept in git with the rest
of the deployment, so a rule change has a review and a history. A rule
describes hardware, and new hardware arrives faster than server releases.

**A member with no rule is not offered.** The catalog offers a member to a
device only if a rule exists and the device satisfies its clauses. So no
exclusion rules are necessary.

**A rule change applies to every entry at once.** A rule is a fact about which
build runs where. If a rule was wrong, it was wrong for every old entry too. So
a rule change can change the member that a name selects for a device. That is
correct, because the earlier selection was wrong.

**The server checks the rules at load.** Every flag must be canonical, and every
flavor must be a flavor that the plan types know. The server also logs each
known flavor that has no rule.

**A gap is visible.** A query that withholds members reports how many it
withheld and why, for example "no rule for `linux-x64-rocm`". The console shows
the members that no rule covers, and the approved devices that get no offer.

**Org entries use the same rules.** An organization cannot add an offer rule.
An org entry for a custom build often has a flavor that no rule covers, so the
short list withholds that member. A plan that names such an entry needs the
explicit `name::flavor` to skip the check in step 2
([below](#open)).

### Which member a name selects

Resolution for a device has two steps:

1. The offer rules remove each member that the device cannot run.
2. The first remaining member in the entry's precedence is the result.

The importer copies a default precedence from the configuration into each entry
that it creates. A change to that default affects only entries created after
the change. The operator who creates a hand-made entry sets its precedence.

A Mac matches only `macos-*` flavors, so one ordered list per entry is
sufficient. On a Linux host with a GPU, the order is a decision. For example,
the CUDA build can come before the CPU build.

**A new member does not become the default for a device that the entry already
served.** Suppose that `llamacpp-v0.7.0` selects `linux-cuda-12-8` on a CUDA host,
and the entry later gets a CUDA 13.4 member. The bare name still selects
`linux-cuda-12-8` on that host. A user who wants the new build selects it with
`::`. A new member at any other position would change what a bare name selects,
so the same plan text would select different builds at different times.

### Fields that the clients do not send yet

An offer rule can test flags that the server derives from a device profile. The
server derives `arch:` and `os_family:` from `device_cpu_arch` and
`device_os_family`, and maps known aliases to canonical values. Clients must
send these fields before the catalog can offer `linux-*` members. The client
change is ticket CAT23. Linux clients report `os:<distribution>`. macOS clients
report `os:macos` and `chip:` flags.

A build that is compiled into the iOS or Android app is a later step. The
client reports it as `runtime:<name>:<build>`, and the server maps that build to
a catalog name. A client never reports a catalog name.

## Names in plans

**A plan can name an entry in place of a runtime table.** The plan gives the
catalog name as a string.

**The plan's organization decides which org entries a name can select.** The
plan's organization is:

- the organization that owns the plan's campaign;
- for a campaign that a user owns, the organization of that user at the time of
  filing;
- for a plan outside every campaign, the organization of the account that files
  it.

Ingestion looks up a bare name first in the plan's organization, and then in
the global scope. So an org entry shadows a global entry of the same name.
`global/<name>` selects the global entry. `<org-slug>/<name>` is accepted only
for the plan's own organization. A name with the slug of another organization
is unknown. A user with no organization, and a plan whose campaign owner has
none, get global names only.

**Ingestion resolves each name, and it does not guess.** Ingestion reads the
variant's eligibility clauses and does not change them. An error that asks the
author to choose is better than a run on a build that the author did not
intend. Resolution has two steps:

1. **The candidate** is the first member in precedence that the catalog can
   offer to a device that the variant admits.
2. **The check:** the catalog must offer the candidate to every device that the
   variant admits. If not, ingestion refuses the plan.

"Every device that the variant admits" has two forms:

- For each device in `clients`, the device's flags must satisfy the candidate's
  offer rule.
- The variant's `requires` and `any_of` must imply the candidate's offer rule:
  each device that satisfies them must satisfy the rule. If ingestion cannot
  show this, it refuses the plan.

For example:

- `clients = [one CUDA host]`: the host is offered the CPU, Vulkan and CUDA
  members. Precedence selects the CUDA member, and the check passes.
- `requires = ["os:macos"]`: the candidate is `macos-arm64`. An Intel Mac
  satisfies `os:macos` and is not offered `macos-arm64`, so ingestion refuses
  the plan. The error names the candidates, for example "`llamacpp-v0.7.0`
  selects `macos-arm64`, but this variant also admits devices that are not
  offered it; add a `requires` or select a member with `::`".

Also:

- If the name is unknown or tombstoned, ingestion refuses the plan.
- If the name is hidden, ingestion accepts the plan with a warning.

A member that the entry gets later goes at the end of the precedence. So it
does not change the candidate of a plan that resolved before.

**Ingestion rewrites the document before it stores it.** Each name becomes its
member's descriptor, with a comment that gives the qualified name. Nothing else in the
document changes. A campaign revision expands the stored document again, so a
revision never resolves a name a second time. Ingestion already rewrites a gated
Hugging Face model in the same way ([plan-ingestion.md](../plan-ingestion.md)).
A document with no names is stored byte for byte, as it is today.

**The jobs are the jobs that the descriptor makes.** The rewritten document is
the document that an author who wrote the descriptor would file. The check in
step 2 runs when the plan is filed, and the job does not carry the offer rule.
So a device can still claim the job when the check no longer holds: a device in
`clients` whose flags change, or a device after an offer rule changes. The same
is true today for a descriptor that an author writes, and the client refuses a
build that it cannot run when it installs it.

**A preview is optional.** A dry run through
[`validate_plan`](../api/plans/validate_plan.md) returns the rewritten document
and stores nothing. The answer to a filing also contains the rewritten document,
so a user who skips the preview still sees what the server stored.

**A revision has two editing modes:**

- **The stored document.** The editor shows the document exactly as stored.
- **The named view.** The editor replaces each descriptor with its catalog
  name. It writes `name::flavor` when
  the bare name would select a different member. It writes `global/<name>` when
  an entry of the plan's organization shadows the global name. On save, the server resolves
  the names again. If the result differs from the stored descriptors, the
  editor shows the difference and asks before it continues.

## Display

**A job row or a result row shows `name::flavor`.** One entry has members that
are materially different builds: a CPU result and a CUDA result of
`llamacpp-v0.7.0` are different measurements. So a row always shows the member,
for example `llamacpp-v0.7.0::linux-cuda-12-8`, also when the entry has one
member. The console can show the flavor in a quieter style, but it always shows
it. The bare name appears only where the console shows a whole entry: the
catalog pages and the short list.

A click shows the full descriptor. The tooltip lists the other names of the
descriptor, also as `name::flavor`. The search in a jobs table matches both
forms. The results CSV has a `runtime_catalog_name` column with
`name::flavor`. A descriptor with no name shows as it does today. The jobs
read gives the names of the digests that it shows in `descriptor_names`
([list_plan_jobs.md](../api/plans/list_plan_jobs.md#the-plan-pages-body)).

**The name that a row shows depends on the reader.**

- A member of an organization sees an entry of that organization as the bare
  `name::flavor`.
- A member sees a global entry that an org entry shadows as
  `global/name::flavor`.
- A reader outside an organization, for example a server admin, sees an entry
  of that organization as `acme/name::flavor`.
- Every reader sees a global entry that nothing shadows as the bare
  `name::flavor`.

So two entries with the same bare name never show the same text to one reader.
A row shows only the names that its reader can read
([above](#scopes)).

**The lookup key is the descriptor digest** without a vault `custom_label`, as
the warehouse stores it in `runtime_descriptor_sha256`. The lookup never changes
a stored digest. It finds a name in two ways:

1. **An exact match.** The digest is the digest of a member.
2. **A verified alias.** The digest is in the alias table, which maps it to one
   member.

**Two descriptors of the same build can have different digests.** A synthetic
failure row has the digest of the raw job spec, and a client row has the digest
of the client's serialization ([storage.md](../storage.md#model_descriptor--runtime_descriptor)).
An author can also write a field that holds its default value. So an exact
match alone does not name every historical row.

**An alias is verified, never asserted.** The catalog adds an alias from an
observed descriptor to a member only if both of these are true:

- The observed descriptor parses as the typed runtime, and the parse ignores no
  key. A key that the typed model does not know blocks the alias, because the
  two builds can differ in that key.
- The parsed value is equal to the member's value.

The scorer adds an alias when it stores a row whose digest has no exact match
and the check passes. `catalog alias --scan` does the same for the rows that
the warehouse has already. An operator cannot add an alias by hand. An alias is
for display only: the warehouse keeps every digest as stored.

**The coverage is measured before the catalog depends on it.** `catalog
coverage` reads the recent result rows and reports three numbers: the share
that an exact match names, the share that an alias would name, and the share
with no name, sorted by cause. The first version runs it against production
data before it commits to the lookup design. If exact matches name most rows,
the alias table can come later.

**Two lookups serve the console:**

- one name, or a list of names, to its entry and members. A name can be bare
  or qualified, and a bare name follows the reader's scopes;
- a list of descriptor digests to their names, as this reader sees them.

## Lists

The two lists answer different questions:

| | Full list | Short list |
|---|---|---|
| Question | "Which names exist?" | "What can I use on this device now?" |
| Device | none | one registered device, required |
| Entries | every entry that the caller can read and that is not tombstoned, hidden entries too | pinned and active entries that the caller can read |
| Members | all members of each entry, in precedence order | for a runtime entry, the member that the device resolves; for a model entry, every member that the device is offered |
| An entry with no member offered to the device | included | left out, and counted as withheld |
| Users | the console's catalog pages | a command-line client, and the console's device page |

The two lists can be one route: a request that names a device gets the short
list.

**The full list comes page by page.** Daily entries make it large: llama.cpp
alone adds about 365 entries a year, with about 21 members each. The list uses
the same conventions as other lists
([httpapi.md](../httpapi.md#13-pagination)): `limit` and `cursor`, and an
answer of `items`, `next_cursor` and `unreadable`.

- **Order:** newest first, by the release time, with the name as the
  tiebreak. The release time never changes, so the cursor is stable.
- **Filters:** scope, kind, runtime type, state and a name prefix. The default
  includes hidden entries, and every scope that the caller can read. A prefix such as `llamacpp-daily-2026-09` also serves the
  search box in the console.
- **Scope:** each item gives its scope, and its name as this caller sees it
  ([above](#display)).
- **Members:** a page gives each entry's member count and flavors.
  `members=true` adds the full descriptors. Without it, 50 entries with all
  their members would be about 250 KiB.

**The short list comes whole.** Expiry limits the number of runtime entries,
so the list needs no pages. Model entries are hand-made, and expiry never hides
a hand-made entry. Their number is the number that operators keep active: an
operator hides an old model entry. If that number grows large, the short list
needs pages.
**The digest lookup** takes a limited number of digests in one request.

**The short list for a device** answers "what can I use?". The caller names a
registered device. The caller must be able to read that device: the reach of
the fleet read applies. The list contains:

- each pinned entry;
- each active entry;

of the global scope and of the caller's organization. A holder of
`edit_any_catalog` does not get the entries of other organizations here: the
short list answers what the caller can name in a plan.

Only the lifecycle state selects an entry. Each entry gives the member that the
device resolves, with the name, the release time, the full descriptor and the
run evidence for that member ([below](#run-evidence)). The answer also reports
the withheld members.

### Run evidence

**Run evidence tells what happened when a member ran. It never selects an
entry.** A run can fail because of the model or the benchmark, not the build.
So a success suggests that a build works, and does not prove it.

**The evidence is about one member, the member that the device resolves.** It
is not about the entry. If the device resolves `llamacpp-v0.7.0` to the CUDA
member, the evidence is about the CUDA member, also when the CPU member has
succeeded on the device. A user who wants the CPU build selects it with
`::linux-x64-cpu`.

| Evidence | Answers |
|---|---|
| the last successful run on this device, and its client version | "has this build succeeded on this device?" |
| the last failed run on this device, and its client version | "has this build failed on this device?" |
| the last successful run on any device, and the number of devices with a success | weaker evidence, and labeled as such |

The catalog does not group devices into classes of hardware. A class would
make the evidence ambiguous again. If a later version needs classes, the flags
that each finished run held (`submission_outcomes.run_flags`) are the input.

**The evidence comes from the result rows.** A warehouse row carries the
`client_id`, the `runtime_descriptor_sha256`, `failed`, the `client_version` and
the times. A member matches a row through its digest or a verified alias. The
evidence is either a query of the warehouse, or a small index that the scorer
keeps current. The index has one row for each digest and `client_id`: the last
success, the last failure and the client version. A rebuild from the warehouse
can make it again, as a backfill makes `submission_outcomes`.

**A broken build becomes visible, and an operator acts on it.** If all the
recent daily builds fail on a platform, the evidence shows the failures. An
operator then pins an entry that works.

## The catalog pages

The console's catalog pages read the catalog. They change only org entries,
and only for a reader who holds `edit_any_catalog`, or `edit_catalog` in the
organization of the entry ([below](#who-changes-the-catalog)). "Catalog" is a
page under **Server** in the console's tree, next to "Model params". It has one tab for runtimes and, in the
second version, one tab for models. The pages show the entries of every scope
that the reader can read.

**The list page** shows the full list. Each row has:

- the name, as the reader sees it ([above](#display));
- a **scope** badge: `global`, or the organization's slug;
- a **kind** badge: `stable`, `daily` or `hand-made`. A hand-made name can look
  like any other name, so the badge is the reliable statement of the kind;
- a **state** badge for `pinned` and `hidden`;
- the release time;
- the time of creation, and the importer, the operator or the account that
  created it;
- the member count, and the flavors;
- a warning when a member has no offer rule.

The page has filters for scope, kind, state, runtime type and name prefix. It shows
the entries with the newest release time first. Hidden entries are not shown until the user selects
them with a filter. A strip at the top shows the last importer run: its time,
its result and the assets that it did not classify.

**The entry page** shows:

- the name, the scope badge, the kind badge, the state badge, the release time
  and the time of creation;
- the members in precedence order. Each member has its flavor, the time that
  it was added, its offer-rule status, the number of approved devices that the
  catalog offers it to, its run evidence on all devices (labeled as such), and
  its descriptor with a "copy as plan TOML" control;
- the other names of the same descriptors, each with its scope and kind badges;
- for an org entry and a reader who holds `edit_any_catalog`, or `edit_catalog`
  in the organization of the entry, the controls to pin, unpin, hide, unhide
  and tombstone the entry. Tombstone asks the reader to type the name again.

The entry page has no form to add a member. The campaign page adds a runtime
or a model to an org entry that already exists
([below](#the-campaign-page)). The HTTP API and the CLI add any member.

### The campaign page

**The campaign page names a descriptor in one step.** On the overview tab, the
progress table shows each runtime descriptor and each model descriptor of the
plan. Each unique descriptor gets a "Create a catalog entry" button. The button
opens a dialog that asks for:

- the name;
- for a model, the quantization label. The format comes from the model type
  ([below](#families-formats-and-quantizations)). The dialog shows each
  quantization that it finds in the file names, repository names, and
  prefixes of the descriptor, such as `Q4_K_M`, `4bit`, or `int4-asym-gq128`.
  A click on one fills the field with its label, such as `q4_k_m`.

The button creates an org entry with one member, the descriptor. The
organization is the plan's organization
([above](#names-in-plans)).

**A name that the organization holds adds a member.** When the dialog opens,
it reads the live entries of the organization. For a runtime, the read also
gives the descriptors of the members. The dialog compares each typed name with
the entries, without regard to case:

- If no entry has the name, the dialog creates an entry.
- If a hand-made entry of the same kind has the name, the dialog adds the
  descriptor to that entry as a new member. The dialog shows a tree of members
  under the entry name. For a model, the tree shows formats and their
  quantizations. For a runtime, it shows flavors. The action of the dialog
  becomes "Add member".
- If an entry of the other kind has the name, the dialog refuses the name.
- For a runtime, the entry must also hold the same build. The runtime must
  differ from a member of the entry only in its flavor
  ([above](#entries-and-members)). If not, the dialog refuses the name.

When the name field loses focus, a name that matches an entry takes the case of
that entry.

A new flavor of a named build thus joins the entry of that build. A new
quantization of a named model joins the entry of that model. The server refuses
a runtime whose flavor matches the flavor of a member of the entry. It also
refuses a model whose format and quantization match those of a member
([below](#families-formats-and-quantizations)). If the read of the entries
fails, the dialog can only create an entry.

The console shows the button only for a
descriptor that has no name in that organization. It shows the button only to
one of these readers:

- a member of that organization that holds `edit_catalog`;
- a reader in no organization that holds `edit_any_catalog`.

So the check for a name never reads the entries of another organization. The
campaign overview gives this organization as `catalog_org_id`, and gives `null`
to every other reader.

**A model descriptor without a commit gets a disabled button.** The button
tells the reader why. Such a descriptor means the newest commit at the time of
the run. A name for today's commit can name a different model, and its digest
does not match the campaign's rows ([below](#members-never-change)).

The console also disables the button for these descriptors, and tells the
reader why:

- a runtime descriptor with a vault source;
- a model with one of these sources or types:
  - a URL;
  - a local file;
  - a local directory;
  - `apple_foundation_text`, which names no files.

The console cannot tell a gated repository from a public one. For a gated
Hugging Face model, the server refuses the entry, and the dialog shows the
reason. A model copied into the organization's vault can have an entry.

## Lifecycle

| State | Shown in the short list | Resolves in a plan | Shown in the console |
|---|---|---|---|
| pinned | yes | yes | yes |
| active | yes | yes | yes |
| hidden | no | yes, with a warning | yes |
| tombstoned | no | no | no |

**Automatic expiry depends on the kind.** It moves an entry from active to
hidden:

- **stable:** The two active or pinned stable entries with the newest release
  times from each upstream do not expire. Other active stable entries expire
  after two months. A fast upstream keeps two
  months of releases. A slow upstream keeps its two newest releases, however
  old they are. A hidden entry does not count as one of the two.
- **daily:** an entry stays active for 14 days after its release.
- **hand-made:** only a person hides it: an operator for a global entry, or a
  member of the organization for an org entry.

The thresholds are configuration. **A pinned entry never expires.** So the
active stable and daily entries are the recent ones, and these entries need no
other limit in the short list.

**Expiry reads the release time, not the time of creation.** The catalog also
applies expiry when it creates an entry. So an entry whose release is already
too old for its kind starts as hidden. A hidden entry still resolves and still
shows, so it still names old results.

**A tombstone is for a mistake,** for example a wrong descriptor or a typo in a
name. A tombstoned name does not resolve and does not show. Nobody can create
the same name again in the same scope.

**The daily importer run also applies automatic expiry.** So expiry needs no
schedule of its own.

## Who changes the catalog

**Who changes an entry depends on its scope.**

| Scope | Changed by | Permission |
|---|---|---|
| global | the host CLI only | none: access to the host is the authority ([cli.md](../cli.md)) |
| organization | the HTTP API, the console, and the host CLI | `edit_catalog` and membership, or `edit_any_catalog` |

**A global entry has no route or page that writes it.** A `pipette-mgmt`
command runs on the host with direct access to storage, and checks no
permission. A global name is visible to every account, and changes to global
entries are rare, so the host is the right authority.

**An org entry is easy to change.** Two permissions control it:

- `edit_catalog` creates and changes the entries of the caller's own
  organization. The `org_member` and `org_admin` permission groups include it.
  A service account can hold it.
- `edit_any_catalog` does the same for every organization. The `server_admin`
  permission group includes it. A service account cannot hold it.

Neither permission writes a global entry. As with every permission, a group
gives its names when an operator grants the group. An account that got
`org_member` before this permission existed needs a grant by name.

The routes are under `/api/v1/orgs/{org_id}/descriptor-catalog`. They create an
entry, add a member, change the state, and tombstone an entry. They do the same
checks as the CLI. An organization that the caller cannot reach answers `404`,
as the other routes under `/api/v1/orgs` do. The organization's audit log
records each write. Each write also logs the change at `info`, with the fields
`catalog_name` and `org_id`.

The CLI has these commands, under one `catalog` command. Each command that names
an entry takes `--org <org>` for an org entry. Without it, the command acts on
the global entry:

| Command | Does |
|---|---|
| `import` | runs the importers, and applies automatic expiry; cron runs it |
| `create <file>` | creates a hand-made entry from a TOML file; `--dry-run` checks the file and writes nothing |
| `add-member <name> <file>` | appends one member from `runtime = { ... }` to a hand-made entry; `--dry-run` writes nothing |
| `refresh <name>` | reads an importer entry's release again, and adds the members that the entry does not have |
| `pin`, `unpin` | moves an entry between pinned and active |
| `hide`, `unhide` | moves an entry between active and hidden |
| `tombstone` | tombstones an entry; it asks for the name again as confirmation, or takes `--yes` for scripts |
| `alias --scan` | adds the verified aliases for the result rows in the warehouse; `--dry-run` writes nothing |
| `coverage` | reports how many recent result rows get a name, and why the others do not |
| `list`, `show` | read the catalog from storage |

The file for `create` gives the name, the members and, as an option, the
release time. Each member is the inline
table that a plan uses for a runtime, and the order of the members is the
precedence. `create` checks the same things that the console would show: the
grammar, the reserved patterns of the global scope, a name that exists or is
tombstoned in the same scope, and a member digest that has a name already in
the same scope. For an org entry, `create` also checks that each vault member
belongs to the organization. `add-member` and `refresh` also check
that each new member is the same build as the members that the entry has.

**The audit log records each command that writes,** as it records every other
host command that writes ([cli.md](../cli.md)). Each command also logs the
change at `info`.

**The server reads the catalog from storage.** A change from the CLI or the
HTTP API is visible in the console and the API without a restart. If the server keeps a copy in
memory, that copy must expire after a short time.

## The importer

The importer is a `pipette-mgmt` subcommand. Cron runs it, as it runs
`dataset-cache probe` ([operations.md](../operations.md)). It has one source
module for each upstream. The llama.cpp source comes first.

**It runs daily at 03:00 UTC.** A daily name can only be fixed after its UTC
day ends. GitHub creates a release before CI uploads its assets, so the
importer also ignores a build that is less than a few hours old.

**A stable entry** comes from a `vX.Y.Z` release. The importer reads the
release's `nightly-tag.txt`, and takes the members from that `b<N>` build.

**A daily entry** is the last `b<N>` build of the UTC day that has every flavor
that the importer tracks. A build with an absent flavor can be a broken build.
If no build of the day is complete, that day gets no entry.

**The importer sorts each asset into one of three classes:**

1. A **known flavor** becomes a member.
2. A **known file that is not a build**, such as `cudart-*`, `ui.tar.gz`,
   `xcframework.zip` or `nightly-tag.txt`, is ignored.
3. An **unknown asset** becomes no member. The import still creates the entry
   from the known flavors, and then the run ends in an error state.

On 2026-10-21, build `b11669` had 7 assets that the flavor table does not know,
for example `ubuntu-cuda-13.4-x64` and `win-vulkan-arm64`. If an unknown asset
stopped the import, every import would fail.

**Each run also checks the active importer entries again.** If a release has an
asset that is now a known flavor, the importer adds that member at the end of
the entry. That is a few stable entries and about 14 daily entries, so the cost
is small. `catalog refresh` does the same for one entry that is not active, for
example after the flavor table learns a flavor.

**Each run leaves a record:** the time, the result, the entries created, the
members added and the unclassified assets. The console shows the records. An alert starts when the
last successful run is too old. The newest entries hide a stopped importer, so
the alert is necessary.

**A first run fills in history:** a stable entry for each release since
`v0.2.0`, and daily entries for recent weeks. Operators also create
hand-made entries for the builds that recent results use most, each with the
release time of its build.

Expiry reads the release time, so most entries from the backfill start as
hidden. For example, on 2026-10-21 `llamacpp-v0.2.0` (released 2026-08-21) is
hidden at once, and only the daily entries of the last 14 days are active. The
other entries still name the old results, which is the purpose of the backfill.
An operator pins an old entry to keep it in the short list.

### Other runtimes

| Runtime | Upstream cadence | What identifies the descriptor | Import |
|---|---|---|---|
| OVMS | about monthly | a GitHub release and a flavor | the same as llama.cpp |
| MLX on macOS, uv vLLM, uv SGLang, OpenVINO GenAI, ONNX GenAI | every 2 weeks to every few months | the full `requirements.txt` body | the importer must compile a lock for each target |
| Docker vLLM, Docker SGLang | about every 2 weeks | an image name and a tag | later |
| the iOS and Android app runtimes | with each app build | the build in the app | later, from the client's flags |

Daily entries apply only to llama.cpp. No other upstream publishes faster than
about every two weeks.

**A uv runtime has no release artifact.** Its identity is a fully pinned
requirements body. To create `mlx-v0.32.2`, the importer compiles a lock for
each target and fixes the result into the entry. The transitive pins change over
time, so the lock must be fixed when the entry is created.

**A Docker tag can move.** The same tag can point at different bytes later, and
a catalog name must not change meaning. The catalog creates Docker entries only
after a descriptor can hold an image digest.

## Models

Models use the same entries, members, lifecycle, lists and display as runtimes.
This section gives the rules that are different for models. Models come in the
second version.

### Families, formats and quantizations

**A model entry is one model, for example `AcmeLM-VL-3B`.** Its members are the
files that serve that model in different formats and quantizations. One entry
per model keeps the list short. The entry page shows the members as a table of
formats and quantizations.

**A model member key has two parts: a format and a quantization.**

- **The format** comes from the model type, through a fixed table:

  | Model type | Format |
  |---|---|
  | `gguf_text`, `gguf_vision` | `gguf` |
  | `mlx` | `mlx` |
  | `torch` | `torch` |
  | `openvino` | `openvino` |
  | `onnx_genai` | `onnx` |

  One entry cannot hold two model types with the same format.
- **The quantization** is a label that the operator gives in the `create` file,
  for example `q4_0`, `q4_k_m`, `4bit`, `int4` or `bf16`. A model descriptor
  usually does not hold its quantization in a field: the quantization is part of
  a file name or a repository name. A label matches `[a-z0-9_]+`.

An entry has at most one member for each format and quantization.

**A quantization belongs to its format.** `q4_0` is a GGUF scheme and `4bit` is
an MLX scheme. Two quantizations in different formats are comparable at most,
never the same. So a plan that compares formats names each member explicitly.

**A speculative pairing is an entry of its own.** `gguf_vision_speculative`,
`mlx_speculative` and `torch_speculative` pair a model with a draft model. The
pair is a different configuration from the model alone, so it gets its own
name, for example `AcmeLM-VL-3B-spec`. Its formats come from the same table.
`apple_foundation_text` names no files, so it gets no name.

### Selectors

**A model selector always has three parts:** `name::format::quantization`, for
example `AcmeLM-VL-3B::gguf::q4_0`. A runtime selector keeps its two parts,
`name::flavor`. The parser reads the entry first, and then reads the rest of
the selector as the entry's kind of member key.

**A model selector must be complete.** A bare model name, or a name and a format
only, does not resolve. For a runtime, precedence selects among builds that the
hardware can run. For a model, a quantization changes the measurement: memory,
speed and accuracy. The catalog does not guess that choice. So a model entry
has no precedence.

A complete selector also makes a new member safe. No partial selector exists,
so a new member cannot change what a selector means.

### Members never change

**Each member pins one commit.** A Hugging Face descriptor without a `revision`
means the newest commit of `main`, which changes over time. So a model member
must hold a commit, never a branch. When the `create` file gives a branch or no
revision, `create` resolves it to the current commit, and prints the commit.

**A new commit is a new entry.** The catalog assumes that a model does not
change. A model that changes gets a new entry under a new name, which an
operator creates by hand.

**"The same model" is the operator's claim.** The members of one model entry
usually come from different repositories, for example `…-GGUF` and
`…-MLX-4bit`, each at its own commit. No check can show that they hold the same
model. For a runtime, the same-build check compares the repository and the tag.
For a model, the operator who creates the entry or adds a member is
responsible. Members are append-only, as for runtimes.

### Public and private models

**Only a public, ungated Hugging Face model gets a global name.** `create` and
`add-member` ask the Hugging Face API about each member's repository, with no
token, and refuse a private or a gated repository. They also refuse a URL, a
local file and a local directory. For a global entry, they also refuse the
vault.

A global name is visible to every account on the server. A global name for a
model that not every reader can see would show that the model exists.

**An org entry names a private or gated model through its vault copy.** Only
the members of the organization read the entry ([above](#scopes)). Ingestion
copies a private or gated model into the vault of the campaign's owner, and
stores no token ([plan-ingestion.md](../plan-ingestion.md#the-import-of-a-gated-model)).
An org entry names that copy if the organization owns it, so the catalog needs
no token. Both org and global entries refuse a Hugging Face descriptor of a
private or gated repository. The `create` command needs a token to pin its
commit. A device needs a token to download the model.

A repository that becomes private or gated after a global entry is created
keeps its name. The name showed nothing that was not public when the operator
created it.

### What a device is offered

**Models have no offer rules.** The catalog offers a model member to a device
if it offers the device a runtime member that can load the model
(`is_compatible`). A device that can run llama.cpp and MLX gets the `gguf` and
`mlx` members of a model entry. A model entry has no precedence, so the short
list shows every model member that the device is offered, not one.

The check counts every member of a pinned or active runtime entry that the
offer rules offer to the device. This includes the members that precedence
does not select. Offer rules exist only for the runtime types with a flavor
table. A model member that only an MLX runtime can load stays withheld until
MLX has offer rules. The withheld reason names the model type: "no offered
runtime loads `mlx`".

### Model names in plans

**A plan can give a model selector in place of a model table.** Ingestion
rewrites the selector to the member's descriptor, with a comment that gives the
selector, as for a runtime. The planner pairs each model with each runtime that
can load it, as it does today.

**A model member with no compatible runtime in its variant is an error.** The
planner refuses such a model today, and a model that came from a selector gets
the same refusal. The error names the selector.

### Display and lists

- A job row, a result row, and a model cell of the campaign's progress table
  show the full model selector, for example `AcmeLM-VL-3B::gguf::q4_0`. The
  lookup key is `model_descriptor_sha256`. The results CSV has a
  `model_catalog_name` column with the selector.
- The filters of the full list name a descriptor type, a runtime or a model,
  and then a runtime type or a model type.
- The entry page shows the model parameter table's counts for each member. The
  member's `model_key` finds the row. The members of one entry can have
  different keys, because they come from different repositories.

### Creating model entries

**Model entries are hand-made only.** No importer follows a model release
stream. An operator creates each global model entry with `catalog create`, and
adds a member later with `catalog add-member`. A member of an organization
creates an org model entry, or adds a model to one, from the campaign page
([above](#the-campaign-page)), or with the HTTP API.

## The first version

- runtimes only, with models in the second version;
- the llama.cpp importer, then OVMS;
- hand-made entries for every other runtime;
- members only for platforms whose flags the clients report;
- a `catalog coverage` report on production data, before the lookup design is
  final;
- a terminology change: call the model-params rows the "model parameter table"
  and reserve catalog terminology for the model entries in the second version.

Scopes come in a third version. The first two versions have global entries
only. The third version adds org entries, the `edit_catalog` permission, the
write routes and the button on the campaign page. Each global entry from the
first two versions stays global.

## What is deliberately not here

- **No global names for vault resources, private models or gated models.** A
  global name for a private artifact would show its existence to every reader.
  An org entry can name the organization's own private artifacts
  ([above](#scopes)).
- **No default quantization.** A model selector always names its format and
  its quantization.
- **No names for flags.** A name covers a descriptor. A name that covered
  `runtime_flags` too would repeat the flags in a longer string.
- **No names for one account.** An entry belongs to the global scope or to one
  organization. An account with no organization uses the global names.
- **No move between scopes.** An org entry never becomes a global entry. An
  operator who wants a global name for the same build creates a global entry
  with the same members, and the two names are synonyms.
- **No change to matching or eligibility.** Job eligibility clauses,
  capability matching and the planner's capability rules stay as they are.
  The offer rules are a separate set, and only the catalog reads them.
- **No names in `requires`.** A plan selects a build when it names an entry, so
  a name in `requires` would add nothing.
- **No writes to global entries over HTTP.** `edit_catalog` and
  `edit_any_catalog` change org entries only. A global name is visible to every
  account, and the host CLI already has the authority.
- **No form on the entry page to add a member.** The campaign page adds a
  runtime or a model to an org entry that already exists. The HTTP API and the
  CLI add any member.
- **No artifact cache.** A copy of each build that ran is a separate design.

## Open

- **Lock targets and offer rules.** For a uv runtime, the compile targets and
  the offer rules describe the same platforms. One configuration section can
  possibly produce both, so that they cannot disagree.
- **Image digests.** A Docker descriptor needs an image digest before the
  catalog can name Docker builds. That is a change to the plan types.
- **A reserved capability namespace.** Adding `catalog` to
  `RESERVED_CAPABILITY_NAMESPACES` now stops clients from using a namespace
  that server-derived flags could need later.
- **A patch release after a newer release.** An upstream can publish
  `v2.4.1` after `v2.5.0`. By release time, `v2.4.1` is the newer of the two,
  so it can push `v2.5.0` out of "the two newest". The two-month window makes
  this rare. If it causes a problem, "newest" can use the version order for
  the upstreams whose versions sort.
- **Runtimes without a flavor.** `::` selects a member by its flavor. Some
  runtime types name their build target in another field: a uv vLLM runtime
  has `build`, and a uv ONNX GenAI runtime has `execution_provider`. Each
  runtime type needs one defined member key for `::` and for display.
- **Client flags.** The clients must send `device_cpu_arch` and
  `device_os_family` before any `linux-*` member is offered. The server
  derives the `arch:` and `os_family:` flags. Client changes are CAT23.
- **An explicit selector and the check.** An org entry for a custom build often
  has a flavor that no offer rule covers. So step 2 of
  [names in plans](#names-in-plans) refuses its bare name, and also
  `name::flavor` if the check applies to it. The proposal is that an explicit
  `name::flavor` skips step 2, because it is the author's choice, the same as a
  descriptor that the author writes. CAT17 decides.
