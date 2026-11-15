# Security Policy

GW2 Enhanced runs inside the game process and updates itself, and its website accepts contributions from players, so
security reports are taken seriously.

## Supported versions

Only the latest release receives fixes. The in-game updater offers new releases automatically.

## Reporting a vulnerability

**Do not open a public issue.** Report privately through GitHub:
[Report a vulnerability](https://github.com/wilssola/GW2Enhanced/security/advisories/new) (repository → Security →
Advisories → "Report a vulnerability").

Please include:

- what is affected (the mod, the updater, the installer/launcher, the website/API) and the version;
- steps to reproduce or a proof of concept;
- the impact you expect (what an attacker could do).

You will get an answer within 7 days. Once a fix is released, the advisory is published and you are credited, unless
you prefer otherwise.

## What is in scope

- **Updates:** the in-game updater and the launcher only install releases whose `SHA256SUMS.txt` carries a valid
  ed25519 signature from the project's release key. Any way to make them install something else is critical.
- **Dictionary releases:** the website signs every dictionary release; bypassing that check is in scope.
- **Website/API** (`app.gw2enhanced.workers.dev`): injection, authentication or rate-limit bypass, exposure of
  stored data (player IP addresses are only stored as salted hashes), cross-site scripting.
- **The mod in the game process:** memory-safety bugs reachable from game data, the network or config files.

## Out of scope

- Anything about Guild Wars 2 itself or ArenaNet's services: report those to ArenaNet.
- Denial of service by sending large volumes of traffic.
- Findings that need an attacker who already controls the player's PC or game folder.
