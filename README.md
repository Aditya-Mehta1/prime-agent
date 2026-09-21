# Agent Projects

## Setup

- macOS/Linux, Node.js 22.8+
- Install-time env: `SAIL_API_KEY`

```bash
./install-local.sh
export PATH="$HOME/.local/bin:$PATH"

cd /path/to/project
agent-projects
```

- Saved key: `~/.agent-projects/sail-api-key` (owner-only)
- State: `~/.agent-projects/` (separate from Prime Agent)

### Update

```bash
cd /path/to/this-checkout
agent-projects shutdown
git pull --ff-only
npm ci
```
