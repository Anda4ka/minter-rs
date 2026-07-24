# Public launch checklist (maintainer)

Do these in order after merging `chore/public-launch-ready`.

## 1. Workflows (required — API cannot write `.github/workflows/`)

Copy from this branch:

```text
docs/github-workflows/ci.yml      → .github/workflows/ci.yml
docs/github-workflows/release.yml → .github/workflows/release.yml
```

Commit on `main` (or include in the merge if you add them locally):

```powershell
copy docs\github-workflows\ci.yml .github\workflows\ci.yml
copy docs\github-workflows\release.yml .github\workflows\release.yml
git add .github/workflows
git commit -m "ci: public launch workflows (fmt/clippy/test + tag release)"
git push
```

## 2. Secret history scan (before Public)

```powershell
# install gitleaks: https://github.com/gitleaks/gitleaks
gitleaks detect --source . --verbose
```

Also: GitHub → Settings → Code security → enable **Secret scanning** + **Push protection** after Public.

If real secrets ever existed in git history, rotate them and consider `git filter-repo` before going public.

## 3. Make repository Public

GitHub → **Settings → General → Danger Zone → Change visibility → Public**.

## 4. About box

- Description: `Local Windows desktop for OpenSea drop mints & raw-contract sniping. Rust + Tauri 2.`
- Website: `https://x.com/AndarkFomo` (or your site)
- Topics: `rust` `tauri` `opensea` `nft` `mint` `seadrop` `ethereum` `windows`
- License: MIT OR Apache-2.0 (should auto-detect)

## 5. Security

- Settings → Code security → **Private vulnerability reporting** → Enable
- Confirm SECURITY.md email is the inbox you monitor

## 6. Delete stale branches

After merge:

```text
docs/add-license
docs/beautify-readme
docs/public-cleanup
fix/wave-b-hygiene
fix/wave-d-audit
chore/public-launch-ready   # after PR merge
```

## 7. Tag v0.1.0 Release

After workflows are on `main` and CI is green:

```powershell
git checkout main
git pull
git tag -a v0.1.0 -m "v0.1.0 — public launch"
git push origin v0.1.0
```

`release.yml` builds Windows zip + SHA256 and attaches them to the GitHub Release.

## 8. Screenshots (recommended before social announce)

Add 2–4 UI screenshots under `.github/assets/` (e.g. `ui-tasks.png`, `ui-mission-control.png`) and link them from README. Unsigned SmartScreen is expected until code signing.

## 9. Announce

Pin: burner-only, no mint guarantee, Windows, link to Release + OPERATOR_GUIDE.
