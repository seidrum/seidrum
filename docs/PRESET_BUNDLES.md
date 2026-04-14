# Preset Bundle System

This document describes Seidrum's preset bundle system for creating, sharing, and importing complete preset configurations including agents, prompts, and plugin declarations.

## Overview

A preset bundle is a self-contained package that includes:

- **Preset definition** (`preset.yaml`): Metadata, plugin/agent requirements, environment variables
- **Agent YAML files**: Custom agents specific to this preset
- **Prompt templates**: Markdown prompts referenced by bundled agents
- **Plugin declarations**: New plugin entries to add to `plugins.yaml`

This allows users to share complete, reproducible configurations that include not just references to existing agents/plugins, but the actual files needed.

## Bundle Format

### Directory Structure

```
my-preset/
├── preset.yaml          # Manifest with metadata and bundle declarations
├── agents/              # Agent YAML definitions to install
│   ├── my-agent.yaml
│   └── another-agent.yaml
├── prompts/             # Prompt files referenced by agents
│   ├── my-agent.md
│   └── another-agent.md
└── README.md            # Optional description
```

### preset.yaml Format

Extended `Preset` struct with new fields:

```yaml
preset:
  # Core fields (existing)
  id: my-custom-preset
  name: My Custom Preset
  description: A preset that does cool things
  icon: sparkles

  plugins:
    required:
      - telegram
      - content-ingester
      - llm-router
      - response-formatter
    recommended:
      - entity-extractor

  agents:
    required:
      - my-custom-agent
      - personal-assistant
    recommended: []

  env_required:
    - key: MY_CUSTOM_KEY
      label: Custom API Key
      help: "Get this from example.com"

  llm_provider: llm-google

  # New fields (for bundles)
  version: "1.0.0"              # Semantic version
  author: username              # Creator/author name
  repository: https://github.com/user/repo

  # Bundle contents declaration
  bundle:
    agents:
      - my-agent.yaml
      - another-agent.yaml
    prompts:
      - my-agent.md
      - another-agent.md
    plugins:                     # New plugin entries
      - name: my-custom-plugin
        binary: seidrum-my-custom-plugin
        env:
          MY_PLUGIN_URL: "${MY_PLUGIN_URL:-http://localhost:9090}"
```

## Type Definitions

### New Types in `seidrum-common/src/config.rs`

```rust
/// Bundle contents declaration for a preset
pub struct PresetBundle {
    /// Agent YAML files included in the bundle (relative paths)
    pub agents: Vec<String>,

    /// Prompt files included in the bundle (relative paths)
    pub prompts: Vec<String>,

    /// New plugin entries to add to plugins.yaml
    pub plugins: Vec<BundledPlugin>,
}

/// A plugin entry to be added to plugins.yaml when a bundle is imported
pub struct BundledPlugin {
    pub name: String,
    pub binary: String,
    pub env: BTreeMap<String, String>,
}
```

### Updated Preset Struct

Added to existing `Preset`:

```rust
pub version: Option<String>,
pub author: Option<String>,
pub repository: Option<String>,
pub bundle: Option<PresetBundle>,
```

## API Endpoints

All endpoints are under `/api/mgmt/` prefix (port 3030 by default).

### List Presets with Source

```
GET /api/mgmt/presets
```

Returns all presets with additional metadata:

```json
[
  {
    "id": "email-assistant",
    "name": "Email Assistant",
    "description": "Email triage and response agent",
    "version": "1.0.0",
    "author": "seidrum",
    "repository": "https://github.com/seidrum/preset-email-assistant",
    "source": "imported",
    "required_plugins": 5,
    "recommended_plugins": 3,
    "required_agents": 1
  }
]
```

Source values:
- `"builtin"`: Core presets included with Seidrum (author: "seidrum", no version)
- `"imported"`: Third-party/community presets (have version field)
- `"custom"`: User-created presets without version info

### Import Preset Bundle

```
POST /api/mgmt/presets/import
Content-Type: application/json
```

Request format (inline import):

```json
{
  "source": "inline",
  "preset_yaml": "preset:\n  id: my-preset\n  ...",
  "agents": {
    "my-agent.yaml": "agent:\n  id: my-agent\n  ..."
  },
  "prompts": {
    "my-agent.md": "# My Agent\n..."
  }
}
```

Response:

```json
{
  "preset_id": "my-preset",
  "preset_name": "My Custom Preset",
  "version": "1.0.0",
  "installed_agents": ["my-agent.yaml"],
  "installed_prompts": ["my-agent.md"],
  "installed_plugins": ["my-custom-plugin"],
  "skipped_agents": [],
  "skipped_prompts": []
}
```

### Export Preset Bundle

```
GET /api/mgmt/presets/{id}/export
```

Returns the complete bundle as JSON:

```json
{
  "preset": {
    "id": "email-assistant",
    "name": "Email Assistant",
    ...
  },
  "agents": {
    "email-triage-agent.yaml": "agent:\n  id: email-triage-agent\n  ..."
  },
  "prompts": {
    "email-triage.md": "# Email Triage\n..."
  }
}
```

### Delete Preset

```
DELETE /api/mgmt/presets/{id}
```

Removes the preset YAML file (not bundled agents/prompts).

Response: `200 OK`

### Apply Preset (Existing)

```
POST /api/mgmt/presets/{id}/apply
```

Existing endpoint - enables plugins and agents from a preset.

## Implementation

### Files Created

1. **`crates/core/seidrum-kernel/src/management/handlers/bundles.rs`**
   - `import_preset()`: Import bundle from inline JSON
   - `export_preset()`: Export preset as bundle
   - `delete_preset()`: Delete custom preset
   - `list_presets_with_source()`: List presets with source metadata
   - `install_bundle()`: Core logic for installing bundle contents

2. **Example Preset Bundle**
   - `config/presets/examples/email-assistant.yaml`: Full example with all features
   - `config/presets/examples/agents/email-triage-agent.yaml`: Bundled agent
   - `config/presets/examples/prompts/email-triage.md`: Bundled prompt
   - `config/presets/examples/README.md`: Usage guide

### Changes to Existing Files

1. **`crates/core/seidrum-common/src/config.rs`**
   - Added `PresetBundle`, `BundledPlugin` types
   - Extended `Preset` with `version`, `author`, `repository`, `bundle` fields

2. **`crates/core/seidrum-kernel/src/management/handlers/presets.rs`**
   - Made `find_preset_file()` public for use by bundles handler

3. **`crates/core/seidrum-kernel/src/management/handlers/mod.rs`**
   - Added `pub mod bundles;`

4. **`crates/core/seidrum-kernel/src/management/routes.rs`**
   - Updated `/api/mgmt/presets` GET to use `list_presets_with_source()`
   - Added `POST /api/mgmt/presets/import` → `bundles::import_preset()`
   - Added `GET /api/mgmt/presets/{id}/export` → `bundles::export_preset()`
   - Added `DELETE /api/mgmt/presets/{id}` → `bundles::delete_preset()`

## Installation Logic

When importing a bundle, the system:

1. **Validates** the preset YAML is well-formed
2. **Installs agent files** from `bundle.agents` to `agents/` directory
   - Skips existing files (doesn't overwrite)
   - Creates directory if needed
3. **Installs prompt files** from `bundle.prompts` to `prompts/` directory
   - Skips existing files
   - Creates directory if needed
4. **Adds plugin entries** from `bundle.plugins` to `plugins.yaml`
   - Only adds if not already present
   - Plugins start disabled by default
5. **Saves preset definition** to `config/presets/{id}.yaml`
   - Always updates (import is idempotent)
6. **Returns installation summary** with counts and skipped files

## Safety & Idempotency

- **Non-destructive**: Doesn't overwrite existing agent/prompt files
- **Idempotent**: Re-importing updates the preset but preserves previous installs
- **Preset-tracked**: Preset definition is always updated to reflect import
- **Audit trail**: Skipped files are reported so users know what wasn't changed

## Examples

### Minimal Preset (No Bundle)

```yaml
preset:
  id: basic-preset
  name: Basic Setup
  description: Minimal preset
  plugins:
    required:
      - content-ingester
      - llm-router
    recommended: []
  agents:
    required:
      - existing-agent
    recommended: []
  env_required: []
  # No bundle: only references existing agents/plugins
```

### Complete Preset with Bundle

See `config/presets/examples/email-assistant.yaml` for a full example that includes:
- Custom agents with bundled files
- Bundled prompts
- Plugin declarations
- Environment variables
- Metadata (version, author, repository)

## Future Enhancements

1. **URL Import**: `"source": "url"` with `https://...tar.gz` or direct YAML URL
2. **Archive Support**: Accept `.tar.gz` bundles with directory structure
3. **Validation**: Schema validation and dependency checking
4. **Versioning**: Handle preset upgrades and version conflicts
5. **Package Registry**: Central repository of community presets
6. **Rollback**: Keep history of installed presets for easy rollback

## Conventions

Following Seidrum conventions:

- Use `tracing` for all logging (info, warn, error)
- `anyhow::Result` for error handling
- No `unwrap()` in production code
- All event structs derive `Serialize, Deserialize, Debug, Clone`
- Scope enforcement on brain queries
- bus event publishing for state changes
