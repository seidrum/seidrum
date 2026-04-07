# Seidrum Preset Bundle Examples

This directory contains example preset bundles that can be imported into Seidrum installations. Each preset is a self-contained configuration that includes agents, prompts, and plugin declarations.

## Email Assistant Preset

The `email-assistant.yaml` is a complete example preset bundle that demonstrates all available features:

- **Bundled Agents**: Includes `email-triage-agent.yaml` which will be installed to the `agents/` directory
- **Bundled Prompts**: Includes `email-triage.md` which will be installed to the `prompts/` directory
- **Plugin Requirements**: Declares required and recommended plugins
- **Environment Variables**: Specifies API keys and configuration needed
- **Metadata**: Version, author, repository for tracking

## Preset Structure

A preset bundle consists of:

```
email-assistant/
├── preset.yaml                    # Manifest with metadata and bundle declarations
├── agents/
│   └── email-triage-agent.yaml   # Agent definitions to bundle
└── prompts/
    └── email-triage.md            # Prompt templates to bundle
```

## Preset File Format

The `preset.yaml` file defines:

```yaml
preset:
  # Identity & Metadata
  id: unique-preset-id
  name: Human-Readable Name
  version: "1.0.0"
  author: creator-name
  repository: "https://github.com/user/repo"
  description: What this preset does

  # Plugin requirements
  plugins:
    required:
      - plugin-name
    recommended:
      - optional-plugin

  # Agent requirements
  agents:
    required:
      - agent-id
    recommended: []

  # Environment variables needed
  env_required:
    - key: API_KEY
      label: API Key
      help: Where to get this value

  # LLM provider preference
  llm_provider: llm-google

  # Bundle contents (for custom presets)
  bundle:
    agents:
      - email-triage-agent.yaml
    prompts:
      - email-triage.md
    plugins:
      - name: custom-plugin
        binary: seidrum-custom-plugin
        env:
          PLUGIN_VAR: value
```

## Using Preset Bundles

### Import via API

```bash
# Import a preset bundle with inline content
curl -X POST http://localhost:3030/api/mgmt/presets/import \
  -H "Content-Type: application/json" \
  -d '{
    "source": "inline",
    "preset_yaml": "..."
    "agents": {
      "email-triage-agent.yaml": "..."
    },
    "prompts": {
      "email-triage.md": "..."
    }
  }'
```

### Export a Preset

```bash
# Export an installed preset as a bundle
curl http://localhost:3030/api/mgmt/presets/email-assistant/export
```

### List All Presets

```bash
# List all presets with source information
curl http://localhost:3030/api/mgmt/presets
```

### Delete a Preset

```bash
# Delete a custom or imported preset
curl -X DELETE http://localhost:3030/api/mgmt/presets/email-assistant
```

## Creating Your Own Preset

To create a custom preset bundle:

1. Create a directory with your preset name
2. Write a `preset.yaml` with required fields
3. Create `agents/` and `prompts/` subdirectories
4. Add your agent YAML files and prompt markdown files
5. Reference them in the `bundle:` section of `preset.yaml`
6. Use the import API to install it

## Preset Sources

Presets are categorized by source:

- **builtin**: Core presets included with Seidrum (author: "seidrum")
- **imported**: Community or third-party presets (have version field)
- **custom**: User-created presets (no version field)

The `list_presets` endpoint returns source information for filtering.

## Notes

- Bundle contents are installed only once; existing files are skipped
- Plugins declared in a bundle start disabled by default
- Agent files are not overwritten if they already exist
- The preset.yaml itself is always installed/updated
- Import is idempotent: re-importing updates the preset definition
