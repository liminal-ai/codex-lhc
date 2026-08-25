> **This is a maintained fork of
> [`openai/codex`](https://github.com/openai/codex).**
>
> **Codex + LHC** keeps the full transcript of a session and serves
> **long-horizon views** with a fidelity ramp: recent work verbatim, older work
> progressively compressed, and the original record preserved underneath.
>
> Compressed spans carry stable turn and message IDs. When a thin view is not
> enough, Codex can call **`get_turns`** or **`get_messages`** to retrieve the
> exact source by ID.
>
> Built on [**LHC (Long Horizon Context)**](https://github.com/liminal-ai/long-horizon-context).
> Product branch **`lhc`**; **`main`** tracks upstream only.
>
> - [**What this fork is**](lhc-docs/README.md) — behavior, LHC concepts, and
>   the Codex integration.
> - [**Install & use**](lhc-docs/INSTALL.md) — release installation, updates,
>   source builds, storage, and verification.
> - [**Current release notes**](https://github.com/liminal-ai/codex-lhc/releases/latest)
>   — changes, compatibility, packages, and checksums.
> - [**LHC project**](https://github.com/liminal-ai/long-horizon-context) — the
>   shared engine and design.
>
> Install the latest fork release on Linux or macOS:
>
> ```sh
> curl -fsSLO https://github.com/liminal-ai/codex-lhc/releases/latest/download/install.sh
> sh install.sh
> ```
>
> On Windows PowerShell:
>
> ```powershell
> Invoke-WebRequest https://github.com/liminal-ai/codex-lhc/releases/latest/download/install.ps1 -OutFile install.ps1
> .\install.ps1
> ```
>
> Releases provide **Linux x86-64/ARM64, Windows x86-64/ARM64, and Apple
> Silicon macOS** packages from one source identity. The installer uses `codex`
> when no Codex command exists and `codex-lhc` for a side-by-side install.
> Official Codex installers and `openai/codex` releases do **not** include LHC.
>
> Everything below is upstream's README. Its install commands install stock
> Codex, not Codex + LHC.

<p align="center"><strong>Codex CLI</strong> is a coding agent from OpenAI that runs locally on your computer.
<p align="center">
  <img src="https://github.com/openai/codex/blob/main/.github/codex-cli-splash.png" alt="Codex CLI splash" width="80%" />
</p>
</br>
If you want Codex in your code editor (VS Code, Cursor, Windsurf), <a href="https://developers.openai.com/codex/ide">install in your IDE.</a>
</br>If you want the desktop app experience, run <code>codex app</code> or visit <a href="https://chatgpt.com/codex?app-landing-page=true">the Codex App page</a>.
</br>If you are looking for the <em>cloud-based agent</em> from OpenAI, <strong>Codex Web</strong>, go to <a href="https://chatgpt.com/codex">chatgpt.com/codex</a>.</p>

---

## Quickstart

### Installing and running Codex CLI

Run the following on Mac or Linux to install Codex CLI:

```shell
curl -fsSL https://chatgpt.com/codex/install.sh | sh
```

Run the following on Windows to install Codex CLI:

```shell
powershell -ExecutionPolicy ByPass -c "irm https://chatgpt.com/codex/install.ps1 | iex"
```

The standalone installers download from `https://releases.openai.com/codex` by default and fall back to GitHub Releases if a metadata or asset download is unavailable. To force GitHub Releases, set `CODEX_INSTALLER_USE_RELEASES_OPENAI_COM` to `false` (`0` and `no` are also accepted):

```shell
curl -fsSL https://chatgpt.com/codex/install.sh | CODEX_INSTALLER_USE_RELEASES_OPENAI_COM=false sh
```

```powershell
$env:CODEX_INSTALLER_USE_RELEASES_OPENAI_COM='false'; irm https://chatgpt.com/codex/install.ps1 | iex
```

Codex CLI can also be installed via the following package managers:

```shell
# Install using npm
npm install -g @openai/codex
```

```shell
# Install using Homebrew
brew install --cask codex
```

Then simply run `codex` to get started.

<details>
<summary>You can also go to the <a href="https://github.com/openai/codex/releases/latest">latest GitHub Release</a> and download the appropriate binary for your platform.</summary>

Each GitHub Release contains many executables, but in practice, you likely want one of these:

- macOS
  - Apple Silicon/arm64: `codex-aarch64-apple-darwin.tar.gz`
  - x86_64 (older Mac hardware): `codex-x86_64-apple-darwin.tar.gz`
- Linux
  - x86_64: `codex-x86_64-unknown-linux-musl.tar.gz`
  - arm64: `codex-aarch64-unknown-linux-musl.tar.gz`

Each archive contains a single entry with the platform baked into the name (e.g., `codex-x86_64-unknown-linux-musl`), so you likely want to rename it to `codex` after extracting it.

</details>

### Using Codex with your ChatGPT plan

Run `codex` and select **Sign in with ChatGPT**. We recommend signing into your ChatGPT account to use Codex as part of your Plus, Pro, Business, Edu, or Enterprise plan. [Learn more about what's included in your ChatGPT plan](https://help.openai.com/en/articles/11369540-codex-in-chatgpt).

You can also use Codex with an API key, but this requires [additional setup](https://developers.openai.com/codex/auth#sign-in-with-an-api-key).

## Docs

- [**Codex Documentation**](https://developers.openai.com/codex)
- [**Contributing**](./docs/contributing.md)
- [**Installing & building**](./docs/install.md)
- [**Open source fund**](./docs/open-source-fund.md)

This repository is licensed under the [Apache-2.0 License](LICENSE).
