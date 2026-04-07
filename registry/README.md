# Seidrum Package Registry

Community packages for the Seidrum AI agent platform.

## Structure

- `index.yaml` — Package index (name, version, kind, description)
- `packages/<name>/` — Package directory
- `packages/<name>/seidrum-pkg.yaml` — Latest manifest per package
- `packages/<name>/versions/<version>.yaml` — Version-specific manifests

## Package Types

- **plugin** — Standalone Seidrum plugin binary (channel handler, tool, etc.)
- **agent** — Standalone agent YAML with prompt template
- **bundle** — Multi-component package (plugins + agents + preset) distributed together
- **skill** — System skill YAML (enrichment, context, etc.)

## Publishing a Package

### From a Plugin/Agent Project

1. Create a `seidrum-pkg.yaml` in your project root
2. Build and create release artifacts (binaries, Docker images)
3. Upload release to GitHub Releases or package registry
4. Run `seidrum pkg publish` from your project directory to push metadata to this registry
5. Or manually fork this repo, add your `packages/<name>/seidrum-pkg.yaml`, and submit a PR

### Manual Steps

1. Fork this repository
2. Create `packages/<your-package-name>/` directory
3. Add `seidrum-pkg.yaml` with package metadata
4. Optionally add `versions/<version>.yaml` for version history
5. Submit a PR to this repository
6. Upon merge, your package becomes discoverable via `seidrum pkg search`

## Using This Registry

This is the default registry. It's automatically configured when you install Seidrum.

### Add a Custom Registry

```bash
seidrum pkg registry add myregistry https://github.com/user/my-registry.git
```

### Search Packages

```bash
seidrum pkg search --kind plugin
seidrum pkg search "ai agent"
```

### Install a Package

```bash
seidrum pkg install seidrum-example-plugin
seidrum pkg install seidrum-example-plugin@0.1.0
```

## Package Manifest Format

See `examples/seidrum-pkg-plugin.yaml`, `examples/seidrum-pkg-bundle.yaml`, and `examples/seidrum-pkg-agent.yaml` for full examples.

### Common Fields

```yaml
package:
  name: package-name                # Must be unique in registry
  version: 0.1.0                    # Semantic versioning
  kind: plugin | agent | bundle | skill
  description: Short description
  author: Your Name
  license: MIT | Apache-2.0 | etc.
  repository: https://github.com/user/repo
  min_seidrum_version: "0.5.0"      # Minimum compatible version
```

### Plugin Package

- `plugin.binary` — Executable name or URL to binary artifacts
- `plugin.consumes` — List of NATS subjects the plugin subscribes to
- `plugin.produces` — List of NATS subjects the plugin publishes to
- `plugin.capabilities` — Tools, commands, or transformations the plugin provides
- `plugin.env` — Environment variables (API keys, config)
- `plugin.artifacts` — Binary downloads per platform

### Agent Package

- `agent.id` — Unique agent ID
- `agent.prompt` — Relative path to prompt template
- `agent.tools` — Capabilities this agent can use
- `agent.scope` — ArangoDB scope constraint (usually `scope_root`)
- `agent.subscribe` — NATS subjects that wake the agent

### Bundle Package

- `bundle.preset.name` — Display name for dashboard
- `bundle.preset.description` — Preset description
- `bundle.preset.icon` — Emoji or icon name
- `bundle.preset.plugins` — Required/recommended plugins
- `bundle.preset.agents` — Required/recommended agents
- `bundle.includes` — What packages this bundle packages
- `bundle.agents` — Inline agent definitions
- `bundle.prompts` — Inline prompt files

## Platform Targets

For plugins with binary artifacts, specify targets:

- `x86_64-apple-darwin` — macOS Intel
- `aarch64-apple-darwin` — macOS ARM (M1+)
- `x86_64-unknown-linux-gnu` — Linux x86_64
- `aarch64-unknown-linux-gnu` — Linux ARM64
- `x86_64-pc-windows-msvc` — Windows

Provide SHA256 checksums for all artifacts.

## Registry Index

The `index.yaml` file at the root lists all packages with their latest version and basic metadata. This file is auto-generated when you publish, but can be manually edited for special cases.

```yaml
packages:
  - name: seidrum-example-plugin
    latest: 0.1.0
    kind: plugin
    description: An example plugin
    author: seidrum
```

## Contributing

Found a great package? Contribute it to the registry:

1. Fork this repo
2. Create a branch: `git checkout -b add/your-package`
3. Add your package manifest to `packages/<name>/seidrum-pkg.yaml`
4. Commit and push: `git push origin add/your-package`
5. Submit a PR with a brief description of what your package does
6. Maintainers will review and merge
7. Your package appears in `seidrum pkg search` after merge

## License

All packages in this registry retain their original licenses. The registry index itself is MIT licensed.
