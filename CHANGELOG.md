# Changelog

## [0.3.0](https://github.com/xxb/herdr-tui/compare/v0.2.0...v0.3.0) (2026-11-13)


### Features

* **mouse:** support horizontal wheel tab switching with inertia & bleed guards ([#41](https://github.com/xxb/herdr-tui/issues/41)) ([#42](https://github.com/xxb/herdr-tui/issues/42)) ([125f9d8](https://github.com/xxb/herdr-tui/commit/125f9d8ec0e7e9d1b5cf14c36ca24ba4513e8e0e))


### Bug Fixes

* **mouse:** isolate WheelZone scopes and guard SpaceMenu wheel drags & modifiers ([#44](https://github.com/xxb/herdr-tui/issues/44)) ([a57fa5d](https://github.com/xxb/herdr-tui/commit/a57fa5d63bd3b3bf8f9f8a88057d6e3d6930acd8)), closes [#43](https://github.com/xxb/herdr-tui/issues/43)
* **tui:** eliminate detach/re-entry screen artifacts, exit on last tab close, and toggle Space menu with ^b b ([#40](https://github.com/xxb/herdr-tui/issues/40)) ([517c002](https://github.com/xxb/herdr-tui/commit/517c002e95b6b2371b4e09da7780feed4c562b4b))
* **usage:** prevent hung libsecret D-Bus calls from wedging agent quota bar ([#37](https://github.com/xxb/herdr-tui/issues/37)) ([8ce778f](https://github.com/xxb/herdr-tui/commit/8ce778fe47ac532b8884e80265f9d638dfc7584e))

## [0.2.0](https://github.com/xxb/herdr-tui/compare/v0.1.0...v0.2.0) (2026-11-13)


### Features

* **build,ci:** add Git build version metadata, Nix version sync, and release-please (Fixes [#33](https://github.com/xxb/herdr-tui/issues/33)) ([#34](https://github.com/xxb/herdr-tui/issues/34)) ([bbde40f](https://github.com/xxb/herdr-tui/commit/bbde40f773f122de5becfd290e184ef4e03ce09b))
* **cli:** prevent accidental nesting in hc, herdr, tmux, zellij, and screen (Fixes [#25](https://github.com/xxb/herdr-tui/issues/25)) ([#26](https://github.com/xxb/herdr-tui/issues/26)) ([ad097c7](https://github.com/xxb/herdr-tui/commit/ad097c7567fd29d15267ec8f3c2b98a2227c8759))
* **hct:** implement multi-tab TUI client for herdr-core ([ac6a091](https://github.com/xxb/herdr-tui/commit/ac6a09154509c1166fe2132369be9bcb4855180f))
* **hct:** implement multi-tab TUI client for herdr-core ([#1](https://github.com/xxb/herdr-tui/issues/1)) ([e2a67b3](https://github.com/xxb/herdr-tui/commit/e2a67b38ba416cd2b1f39f6dbfa677f147709f29))
* **touch:** optimize touch & iPad SSH interactions ([#4](https://github.com/xxb/herdr-tui/issues/4)) ([d9c7c83](https://github.com/xxb/herdr-tui/commit/d9c7c8309e9839fccecbe41d2e7caa9795b7c723))
* **ui:** adopt classic herdr Catppuccin palette for active tab highlight (Fixes [#29](https://github.com/xxb/herdr-tui/issues/29)) ([#30](https://github.com/xxb/herdr-tui/issues/30)) ([6abf832](https://github.com/xxb/herdr-tui/commit/6abf8323fb08d460c0955b8e41167304d5145d79))
* **ui:** dual floating + crosses, top space/tab/+ bar, space popover, and 1-row bottom agent status bar ([#11](https://github.com/xxb/herdr-tui/issues/11)) ([31c4945](https://github.com/xxb/herdr-tui/commit/31c4945e649f31dd6262b204cca707da8b4eec2f))
* **ui:** floating draggable semi-transparent + cross D-pad with centered paste ([#8](https://github.com/xxb/herdr-tui/issues/8)) ([373f9e8](https://github.com/xxb/herdr-tui/commit/373f9e806e8d37f0236af1cc6ee46dcd8e60048d))
* **ui:** fold inactive top-bar tabs via accordion and drop redundant circles (Fixes [#27](https://github.com/xxb/herdr-tui/issues/27)) ([#28](https://github.com/xxb/herdr-tui/issues/28)) ([d5f4220](https://github.com/xxb/herdr-tui/commit/d5f42208f72ddb2ed9894ed5d58e8384a18b7612))
* **ui:** reduce Top Tab Bar height to 2 rows and add far-left Detach button (Fixes [#17](https://github.com/xxb/herdr-tui/issues/17)) ([#18](https://github.com/xxb/herdr-tui/issues/18)) ([7f879ab](https://github.com/xxb/herdr-tui/commit/7f879abb2f9de5c5739f444ce009bdcebd4023f1))
* **ui:** render single-row focused-agent quota bar with used percentage (Fixes [#10](https://github.com/xxb/herdr-tui/issues/10)) ([#12](https://github.com/xxb/herdr-tui/issues/12)) ([c3a3d3e](https://github.com/xxb/herdr-tui/commit/c3a3d3ec3636bbff259be45bceecb2d55b1cb56f))
* **ui:** span dim underline-color across full Top Tab Bar with active tab highlighted ([#24](https://github.com/xxb/herdr-tui/issues/24)) ([070ad60](https://github.com/xxb/herdr-tui/commit/070ad60612ac623e1d7ba881d3aa89a76cc560a5))


### Bug Fixes

* **hct:** address codex review findings (CAS rebase, event channel, resize, drain bound, fuzzy scroll) ([0845282](https://github.com/xxb/herdr-tui/commit/08452825f909b7e3bf92ff15fd95344e011dd0e0))
* **hct:** bind modal actions to target IDs and reject undecodable doc versions ([6b98fd9](https://github.com/xxb/herdr-tui/commit/6b98fd9b972970e3b056f8edd8d8824c2c4f755a))
* **hct:** encode RPC labels as object, handle created/missed events, preserve active space ID ([6f5d4f7](https://github.com/xxb/herdr-tui/commit/6f5d4f7cec0c24367dbe5b62bf4e109dea06a4f5))
* **hct:** preserve fuzzy selection by tab ID, add terminal RAII guard, truncate oversized tab labels ([20778c9](https://github.com/xxb/herdr-tui/commit/20778c9c6931bc77d851c1ee8f0f2abdd96a23ec))
* **hct:** preserve relocated remote tabs on CAS rebase and encode non-alphabetic prefix keys ([33abd09](https://github.com/xxb/herdr-tui/commit/33abd095f69a90b9380a1e22867ee734de3124e9))
* **hct:** refresh foreground cwd and resolve hit/fuzzy targets by stable IDs ([ddb6ac8](https://github.com/xxb/herdr-tui/commit/ddb6ac82e2f201ff1468b0f5ab935abc9c4e0a9b))
* **keys:** preserve combined modifiers when encoding cursor and editing keys ([#1](https://github.com/xxb/herdr-tui/issues/1)) ([d5c4e4b](https://github.com/xxb/herdr-tui/commit/d5c4e4bb6da4c833e353826cdb79903bf38cebc6))
* **model:** confirm terminal death via terminal.list before pruning unseen tabs ([2f8fe83](https://github.com/xxb/herdr-tui/commit/2f8fe8349c816bf1e001d833255dfd3e1010ff90))
* **model:** deduplicate auto-adopted tabs when rebasing create_new_tab/space ([fe4237e](https://github.com/xxb/herdr-tui/commit/fe4237e179026b6f34db13ad45b31cbcb556dc04))
* **perf:** eliminate event-loop RPC stalls, redundant redraws, and rendering allocations ([#16](https://github.com/xxb/herdr-tui/issues/16)) ([1d61216](https://github.com/xxb/herdr-tui/commit/1d612161bbf093be17e06a36bc16c9638b02909a))
* **ui:** make Top Tab Bar compact 1-row and underline emphasized pills ([#22](https://github.com/xxb/herdr-tui/issues/22)) ([5ee393c](https://github.com/xxb/herdr-tui/commit/5ee393c02b5ce2f1250f5daba36c921a22e55eb2))
* **ui:** vertically center 2-row Top Tab Bar labels and move gloss line underneath ([#20](https://github.com/xxb/herdr-tui/issues/20)) ([2cdced6](https://github.com/xxb/herdr-tui/commit/2cdced6e2b9300df820bd4db2777efc6ed9e2272))
* **vt:** translate ECMA-48 HVP (CSI f) and cursor sequences for btop (Fixes [#13](https://github.com/xxb/herdr-tui/issues/13)) ([#14](https://github.com/xxb/herdr-tui/issues/14)) ([0af8ceb](https://github.com/xxb/herdr-tui/commit/0af8ceb2f85e5a25e50d8f49a5e7f1b8c7ebd1c2))


### Performance Improvements

* **terminal:** speed up attach and large multi-line output loading ([#6](https://github.com/xxb/herdr-tui/issues/6)) ([dd1f19f](https://github.com/xxb/herdr-tui/commit/dd1f19f174f363da2168d586279f90adb127f367))
