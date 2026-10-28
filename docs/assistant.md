# The local assistant

The server can run a small language model that reads the bridge's status report and recent log
lines and answers questions about them, in plain language, in the terminal. Nothing leaves the
machine. The model is **Llama 3.2 3B Instruct** ("Built with Llama", see
[../assistant/MODEL-LICENSE.md](../assistant/MODEL-LICENSE.md)), served by
[llama.cpp](https://github.com/ggml-org/llama.cpp) on the CPU.

```sh
sudo bridge-assistant
```

It prints the status report, starts the model, and waits for a question. The first answer takes
about a minute on four cores while the model reads the report; later answers come faster. An empty
line or Ctrl-D ends the session and stops the model, which frees its memory (about 3.5 GB while
running).

## What it can and cannot do

It can explain what the status report shows, say what a log line means, and tell you which of the
installer's commands to run next. Its knowledge of this bridge comes from
[../assistant/RUNBOOK.md](../assistant/RUNBOOK.md), which is also the operator's own reference;
read that page first, and the assistant will be repeating it to you.

It has **no tools**. It cannot run commands, read files, change anything, or move money. When it
suggests a command, you type it. That is deliberate: the machine holds hot keys, and a small model
is confidently wrong often enough that nothing it says should be executed unread.

It cannot know anything that happened after it was trained, including new vulnerabilities. Security
updates are applied by `unattended-upgrades` through the VPN; the status report shows what is
pending. The assistant can explain that report, not replace it.

For a hard problem, the status report is designed to be safe to paste into any larger model or to
anyone helping you: it contains no keys, no mnemonic, no customer addresses, and redacts URLs in
recent error lines. The assistant redacts URLs from its own journal excerpt too. Never paste
`backend.env`, `params.conf`, or a journal excerpt you have not read.

## How it is kept on the machine

- The model process runs as its own user, `bridgeassist`, which can read its own directory and
  nothing else: not `/etc/ecash-bridge`, not the database, not the source.
- Its systemd unit denies every network address except loopback (`IPAddressDeny=any`,
  `IPAddressAllow=127.0.0.0/8`). It listens on `127.0.0.1:3902` and can open no connection
  anywhere, whatever a prompt asks of it. The firewall is a second wall behind that.
- It is started by `bridge-assistant` and stopped when that exits; it is not enabled at boot. A
  four-hour cap stops it anyway if the terminal died.
- It is given text only: the status report, log lines matching error markers, and the runbook.
  Those are produced by `install.sh status` running as root; the model never runs as root.

## The model file

The model is one file, pinned by SHA-256 in [../assistant/MODEL.conf](../assistant/MODEL.conf).
It is not in git. `make-bundle.sh` places it in `bundle/model/`, and `install.sh` verifies the hash
before installing it to `/opt/ecash-bridge/assistant/model.gguf`. To run without the assistant,
make the bundle with `--no-assistant`; `bridge-assistant` then says that no model is installed.

Changing the model means changing `MODEL.conf` and the license file together, in the repository,
and rebuilding the bundle. There is no path by which a different model arrives on the server.
