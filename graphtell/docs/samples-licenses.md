# Sources and licenses of the sample codebases

`samples/` holds **sample codebases for GraphTell to analyze and demonstrate on** -- it is not this
product's own source. Most are third-party open-source projects (licensed under their own terms),
used only as test material and demo assets.

> If anything below disagrees with the upstream repository's statement, **the upstream repository
> wins**.

## Inventory

| Directory | Nature | Upstream | License | License text |
| --- | --- | --- | --- | --- |
| `frontend-backend-link` | **synthetic fixture made by this project** (packages `synthetic/frontend`, `synthetic/backend`) | — | owned by this project | no third-party grant needed |
| `hackathon-starter` | third-party OSS | https://github.com/sahat/hackathon-starter | MIT © Sahat Yalkabov | ✅ shipped upstream (`samples/hackathon-starter/LICENSE`) |
| `nestjs-realworld-example-app` | third-party OSS | https://github.com/lujakob/nestjs-realworld-example-app | ISC | ⚠️ not shipped upstream; standard text added per the `package.json` declaration |
| `typescript-starter` | third-party OSS | https://github.com/nestjs/typescript-starter | MIT | ⚠️ not shipped upstream; standard text added per the `package.json` declaration |
| `php-projects/laravel-starter` | third-party OSS (official Laravel skeleton) | https://github.com/laravel/laravel | MIT © Taylor Otwell | ⚠️ standard text added per the `composer.json` declaration |
| large third-party e-commerce systems | — | non-standard permissive | **not included in this repo**: set `GRAPHTELL_SAMPLE_DIR` and supply them yourself |

## Notes

- **Why the license texts were added**: both MIT and ISC permit redistribution, provided the
  copyright notice and license text travel along. The three samples marked ⚠️ above ship no
  `LICENSE` file upstream, so the corresponding license's standard text was added here per the
  upstream `package.json` / `composer.json` declaration; copyright stays with the upstream authors.
- **`frontend-backend-link` is a self-made fixture**: written by this project to cover cross-end
  chains like "frontend ↔ backend" and semantic nodes such as `Cache` / `Store` / `ConfigKey`. It
  contains no third-party code, so it is free to use in the public demo.
- **Large third-party e-commerce systems are not vendored**: their licenses are not
  standard permissive ones (several lean commercial / open-core), so this repo does not distribute
  their source; related tests and docs skip automatically when the samples are absent.
- **Were the samples modified?** Samples are taken essentially as-is from upstream; some may carry
  this project's own `.graphtell/aliases.json` (project-level intent alias config -- a config file
  of this project, not upstream content).
- **The public demo (`docs/demo/`)**: shows GraphTell's **analysis results** over these samples
  (graph structure, rule violations, recall context), and labels each sample's name, upstream link
  and license on the page. The `recall` context pack quotes small source excerpts from the samples,
  which is excerpt quotation, given here together with attribution.
