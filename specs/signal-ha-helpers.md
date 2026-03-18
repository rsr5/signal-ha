# `signal-ha-helpers` — Crate Design Plan

> **Status**: Draft  
> **Date**: 2026-03-17  
> **Goal**: Let automations self-provision and manage their own HA helper
> entities, eliminating the need for external YAML packages.

---

## Motivation

Today every signal-ha automation depends on HA packages (YAML) to
pre-create its `input_boolean`, `input_number`, `input_text`, etc.
entities. This creates three problems:

1. **Parallel maintenance** — every entity exists in two places (the YAML
   package that creates it, and the Rust code that references it).
2. **No cleanup** — removing an automation leaves orphaned helpers in HA.
3. **Non-portable** — the automation can't run on a fresh HA instance
   without first importing the package.

**Scale of the problem today:**

| Automation | input_boolean | input_number | input_text | input_select | input_datetime | Total |
|-----------|:---:|:---:|:---:|:---:|:---:|:---:|
| bathroom-lights | 4 | 8 | 1 | — | 5 | 18 |
| office-lights | 6 | 4 | 7 | 2 | — | 19 |
| living-room-lights | 3 | 1 | 4 | 1 | — | 9 |
| kitchen | 2 | 1 | 3 | — | — | 6 |
| porch-lights | 1 | — | — | — | — | 1 |
| agile-tesla | 1 | — | — | — | — | 1 |
| **Total** | **17** | **14** | **15** | **3** | **5** | **54** |

All 54 entities are currently defined in YAML packages under
`appdaemon/apps/*/packages/`. With this crate, each automation declares
what it needs and the library ensures they exist at startup.

---

## Core Concept: `HelperManifest`

Each automation declares a **manifest** — a list of helpers it needs. On
startup it calls `manifest.ensure(&client)` which:

1. Lists all existing helpers of each type (one WS call per type).
2. For each declared helper, either creates it or updates it to match.
3. Optionally assigns area, labels, and categories via the entity registry.
4. Returns a `ProvisionedManifest` with the resolved entity IDs.

On shutdown (optional): `manifest.cleanup(&client)` deletes helpers that
were created by this manifest (tracked by label).

```
Before:          HA packages (YAML) ──creates──▷ helpers ◁──reads── automation
After:   automation ──declares manifest──▷ signal-ha-helpers ──ensures──▷ helpers
```

---

## API Design

### Layer 1: Low-Level Generic CRUD

A thin typed wrapper around `send_raw` for any helper domain. Follows the
`DashboardManager` pattern exactly.

```rust
/// Generic CRUD for any `DictStorageCollectionWebsocket` domain.
pub struct HelperCollection<'a> {
    client: &'a HaClient,
    domain: &'static str,    // e.g. "input_number"
}

impl<'a> HelperCollection<'a> {
    pub fn new(client: &'a HaClient, domain: &'static str) -> Self;

    /// List all helpers of this domain.
    /// WS: `{domain}/list`
    pub async fn list(&self) -> Result<Vec<HelperItem>>;

    /// Create a new helper. Returns the created item (includes generated `id`).
    /// WS: `{domain}/create`
    pub async fn create(&self, fields: Value) -> Result<HelperItem>;

    /// Update an existing helper by storage ID.
    /// WS: `{domain}/update` with `{domain}_id`
    pub async fn update(&self, id: &str, fields: Value) -> Result<HelperItem>;

    /// Delete a helper by storage ID.
    /// WS: `{domain}/delete` with `{domain}_id`
    pub async fn delete(&self, id: &str) -> Result<()>;

    /// Subscribe to changes. Returns a receiver that yields
    /// `CollectionChange` events (added/updated/removed).
    /// WS: `{domain}/subscribe`
    pub async fn subscribe(&self) -> Result<broadcast::Receiver<CollectionChange>>;
}

/// A single helper item as returned by HA.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelperItem {
    pub id: String,
    pub name: String,
    #[serde(flatten)]
    pub fields: Value,
}

/// A change event from a subscription.
#[derive(Debug, Clone)]
pub struct CollectionChange {
    pub change_type: ChangeType,  // Added, Updated, Removed
    pub item_id: String,
    pub item: HelperItem,
}

pub enum ChangeType {
    Added,
    Updated,
    Removed,
}
```

This layer is domain-agnostic. It works for all 9 helper types (and any
future helper HA adds) with zero code changes.

### Layer 2: Typed Helper Builders

Type-safe builders for each helper domain. These produce `Value` payloads
for Layer 1, with compile-time enforcement of required fields and
domain-specific constraints.

```rust
/// Type-safe builder for input_number helpers.
pub struct InputNumber {
    name: String,
    min: f64,
    max: f64,
    initial: Option<f64>,
    step: Option<f64>,
    icon: Option<String>,
    unit_of_measurement: Option<String>,
    mode: Option<InputNumberMode>,  // Slider | Box
}

impl InputNumber {
    /// Required fields in the constructor.
    pub fn new(name: impl Into<String>, min: f64, max: f64) -> Self;

    // Optional field setters (builder pattern)
    pub fn initial(self, v: f64) -> Self;
    pub fn step(self, v: f64) -> Self;
    pub fn icon(self, v: impl Into<String>) -> Self;
    pub fn unit(self, v: impl Into<String>) -> Self;
    pub fn mode(self, v: InputNumberMode) -> Self;

    /// Validate constraints and produce the JSON payload.
    fn to_create_value(&self) -> Result<Value>;
}

// Enum for the two input_number display modes.
pub enum InputNumberMode { Slider, Box }
```

Same pattern for all 9 types:

| Builder struct | Required constructor args | Notable optionals |
|---------------|--------------------------|-------------------|
| `InputBoolean` | `name` | `initial`, `icon` |
| `InputNumber` | `name`, `min`, `max` | `initial`, `step`, `unit`, `mode`, `icon` |
| `InputText` | `name` | `min_len`, `max_len`, `initial`, `pattern`, `mode`, `unit`, `icon` |
| `InputSelect` | `name`, `options` | `initial`, `icon` |
| `InputDatetime` | `name` | `has_date`, `has_time`, `initial`, `icon` |
| `InputButton` | `name` | `icon` |
| `Counter` | `name` | `initial`, `min`, `max`, `step`, `restore`, `icon` |
| `Timer` | `name` | `duration`, `restore`, `icon` |
| `Schedule` | `name` | `monday`..`sunday` (each: `Vec<TimeRange>`), `icon` |

Each builder implements a shared trait:

```rust
pub trait HelperSpec: Sized {
    /// The HA domain (e.g. "input_number").
    const DOMAIN: &'static str;

    /// Unique name — used to find or create the helper.
    fn name(&self) -> &str;

    /// Produce JSON for the create/update WS call.
    fn to_value(&self) -> Result<Value>;
}
```

### Layer 3: Entity Registry Operations

After creating a helper, its entity needs area/label/category assignment.
This wraps the `config/entity_registry/*` commands.

```rust
pub struct EntityRegistry<'a> {
    client: &'a HaClient,
}

impl<'a> EntityRegistry<'a> {
    pub fn new(client: &'a HaClient) -> Self;

    pub async fn get(&self, entity_id: &str) -> Result<EntityEntry>;
    pub async fn get_many(&self, entity_ids: &[&str]) -> Result<Vec<EntityEntry>>;
    pub async fn list(&self) -> Result<Vec<EntityEntry>>;

    pub async fn update(&self, entity_id: &str, updates: EntityUpdate) -> Result<EntityEntry>;
    pub async fn remove(&self, entity_id: &str) -> Result<()>;
}

/// Fields that can be updated on an entity registry entry.
#[derive(Default)]
pub struct EntityUpdate {
    pub name: Option<String>,
    pub icon: Option<String>,
    pub area_id: Option<String>,         // None = remove from area
    pub labels: Option<Vec<String>>,
    pub disabled_by: Option<DisabledBy>, // None = enable
    pub hidden_by: Option<HiddenBy>,     // None = unhide
    pub aliases: Option<Vec<String>>,
    pub new_entity_id: Option<String>,   // rename
}
```

### Layer 4: Label & Area Registry

Automations may need to ensure their label/area exists before assigning
entities to them.

```rust
pub struct LabelRegistry<'a> { client: &'a HaClient }

impl<'a> LabelRegistry<'a> {
    pub async fn list(&self) -> Result<Vec<Label>>;
    pub async fn create(&self, name: &str, color: Option<&str>,
                        description: Option<&str>, icon: Option<&str>) -> Result<Label>;
    pub async fn delete(&self, label_id: &str) -> Result<()>;
    /// Ensure a label exists (create if missing, return existing if found).
    pub async fn ensure(&self, name: &str, color: Option<&str>,
                        icon: Option<&str>) -> Result<Label>;
}

pub struct AreaRegistry<'a> { client: &'a HaClient }

impl<'a> AreaRegistry<'a> {
    pub async fn list(&self) -> Result<Vec<Area>>;
    pub async fn create(&self, name: &str, floor_id: Option<&str>,
                        icon: Option<&str>) -> Result<Area>;
    pub async fn ensure(&self, name: &str) -> Result<Area>;
}
```

### Layer 5: The Manifest — Declarative Self-Provisioning

This is the high-level API that automations actually use. An automation
declares all the helpers it needs, and the manifest reconciles them.

```rust
pub struct HelperManifest {
    /// Identifies this automation (used as a label for tracking ownership).
    automation_id: String,
    /// Optional area to assign all helpers to.
    area: Option<String>,
    /// The helpers to ensure exist.
    helpers: Vec<Box<dyn HelperSpec>>,
}

impl HelperManifest {
    pub fn new(automation_id: impl Into<String>) -> Self;

    pub fn area(self, area: impl Into<String>) -> Self;

    /// Add a helper to the manifest.
    pub fn helper(self, spec: impl HelperSpec + 'static) -> Self;

    /// Ensure all helpers exist and are configured correctly.
    /// Creates missing helpers, updates changed ones, leaves matching ones alone.
    /// Assigns the `signal_ha:{automation_id}` label to all managed entities.
    /// Returns a lookup map: helper name → entity_id.
    pub async fn ensure(&self, client: &HaClient) -> Result<ProvisionedManifest>;

    /// Remove all helpers owned by this manifest.
    pub async fn cleanup(&self, client: &HaClient) -> Result<()>;
}

/// The result of manifest provisioning — a map of helper names to entity IDs.
pub struct ProvisionedManifest {
    entities: HashMap<String, String>,  // "bathroom_off_delay_minutes" → "input_number.bathroom_off_delay_minutes"
}

impl ProvisionedManifest {
    /// Get entity ID by helper name. Panics if not found (validated at ensure time).
    pub fn entity_id(&self, name: &str) -> &str;
}
```

### Example: Automation Usage

```rust
// In bathroom-lights main.rs, at startup:

let manifest = HelperManifest::new("bathroom-lights")
    .area("Bathroom")
    .helper(InputBoolean::new("bathroom_toothbrush_lighting_enabled")
        .icon("mdi:toothbrush"))
    .helper(InputBoolean::new("bathroom_enable_ct_by_lux"))
    .helper(InputBoolean::new("bathroom_require_low_lux"))
    .helper(InputBoolean::new("bathroom_enable_mirror_daytime"))
    .helper(InputDatetime::new("bathroom_night_start_time")
        .has_time(true)
        .initial("22:00:00"))
    .helper(InputDatetime::new("bathroom_night_end_time")
        .has_time(true)
        .initial("06:00:00"))
    .helper(InputNumber::new("bathroom_day_brightness_pct", 0.0, 100.0)
        .step(5.0)
        .initial(100.0)
        .unit("%")
        .icon("mdi:brightness-percent"))
    .helper(InputNumber::new("bathroom_off_delay_minutes", 0.0, 60.0)
        .step(1.0)
        .initial(5.0)
        .unit("min")
        .icon("mdi:timer-outline"))
    .helper(InputText::new("bathroom_lighting_reason")
        .icon("mdi:text-box-outline")
        .max_len(255));

let provisioned = manifest.ensure(&client).await?;

// Now use the resolved entity IDs:
let reason_entity = provisioned.entity_id("bathroom_lighting_reason");
// → "input_text.bathroom_lighting_reason"
```

This replaces the entire `appdaemon/apps/bathroom_lights/packages/bathroom_lights.yaml`
file. The automation is now self-contained.

---

## Reconciliation Logic (`ensure`)

The `ensure()` method follows a predictable reconciliation strategy:

```
For each helper type in the manifest:
  1. Call `{domain}/list` to get all existing helpers of this type.
  2. Build a lookup map: name → existing item.
  3. For each declared helper:
     a. If it exists and fields match → skip (no-op).
     b. If it exists but fields differ → call `{domain}/update`.
     c. If it doesn't exist → call `{domain}/create`.
  4. After create/update, resolve the entity_id ({domain}.{slugified_name}).
  5. Update entity registry: set area, add label `signal_ha:{automation_id}`.

For cleanup():
  1. Search entity registry for all entities with label `signal_ha:{automation_id}`.
  2. Delete each helper via `{domain}/delete`.
  3. Remove the label.
```

**Key decisions:**

- **Match by name, not by ID.** HA generates storage IDs internally (e.g.
  slug of the name). The name is the stable key that the automation author
  controls.
- **Ownership via labels.** Each managed entity gets a `signal_ha:<id>`
  label. This makes cleanup safe — we only delete things we created.
- **Update is conservative.** Only fields that differ are sent. HA's update
  endpoint is partial (missing fields = no change).
- **No delete on ensure.** If the manifest shrinks (a helper is removed
  from the code), `ensure()` does NOT delete the old one. Only `cleanup()`
  deletes. This prevents accidental data loss during development.

---

## Crate Structure

```
signal-ha/crates/signal-ha-helpers/
├── Cargo.toml
└── src/
    ├── lib.rs              // pub use everything
    ├── collection.rs       // HelperCollection<'a> — generic CRUD (Layer 1)
    ├── helpers/
    │   ├── mod.rs          // HelperSpec trait + re-exports
    │   ├── input_boolean.rs
    │   ├── input_number.rs
    │   ├── input_text.rs
    │   ├── input_select.rs
    │   ├── input_datetime.rs
    │   ├── input_button.rs
    │   ├── counter.rs
    │   ├── timer.rs
    │   └── schedule.rs
    ├── registry/
    │   ├── mod.rs          // re-exports
    │   ├── entity.rs       // EntityRegistry (Layer 3)
    │   ├── label.rs        // LabelRegistry (Layer 4)
    │   └── area.rs         // AreaRegistry (Layer 4)
    └── manifest.rs         // HelperManifest + ProvisionedManifest (Layer 5)
```

**Dependencies:** Only `signal-ha` (for `HaClient`), `serde`, `serde_json`, `tracing`, `tokio`.

---

## What This Externalises

With this crate, an automation can fully manage its HA footprint:

| Capability | Before | After |
|-----------|--------|-------|
| Create helpers | YAML package (manual) | `manifest.ensure()` at startup |
| Update helpers | Edit YAML + reload HA | Change Rust code, redeploy |
| Delete helpers | Manual HA UI cleanup | `manifest.cleanup()` |
| Assign areas | YAML or HA UI | Declarative in manifest |
| Label entities | HA UI only | Automatic `signal_ha:*` labels |
| Discover entity IDs | Hardcode strings | `provisioned.entity_id("name")` |
| Works on fresh HA | Import packages first | Just start the automation |

**The YAML packages become unnecessary.** Automations are fully
self-contained Rust binaries that declare, provision, and manage their own
HA entities.

---

## Migration Path

1. **Build the crate.** Implement layers 1–5 as described above.
2. **Add manifests to one automation** (e.g. porch-lights — only 1 helper).
   Verify it creates the entity, matches the existing one, doesn't
   duplicate.
3. **Migrate remaining automations** one at a time. After each migration,
   delete the corresponding YAML package.
4. **Delete `appdaemon/apps/*/packages/`** once all automations are
   migrated.

---

## Future Extensions (Not in Scope Now)

- **`DashboardManager` integration** — manifest could declare a dashboard
  card alongside its helpers, auto-creating a debugging dashboard.
- **Automation/script CRUD** — these use REST, not WS. Could add HTTP
  methods to `HaClient` later.
- **Device registry grouping** — create a virtual device per automation
  that all its entities belong to.
- **Schema diffing** — detect when an automation's helper schema changes
  and emit a structured migration log.
- **Category/floor registries** — straightforward to add when needed (same
  pattern as label/area).
