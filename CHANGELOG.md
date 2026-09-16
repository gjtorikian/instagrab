## [0.4.0](https://github.com/gjtorikian/instagrab/compare/v0.3.0...v0.4.0) (2026-09-16)

### Features

* capture and download profile pictures ([#4](https://github.com/gjtorikian/instagrab/pull/4))

### Documentation

* fix config sample and cron logfile bootstrap ([fa60138](https://github.com/gjtorikian/instagrab/commit/fa601388bb955eec1f5855bd725dc0b91f8d65ff))

### Miscellaneous Chores

* **deploy:** start daily scan at 18:00 for measured ~9.5h runtime ([871b962](https://github.com/gjtorikian/instagrab/commit/871b9624e1c00a1420b971521ee97c6aec9eef5f))
* **deploy:** replace cron with randomized systemd timers ([9cef066](https://github.com/gjtorikian/instagrab/commit/9cef066b3a09f17a47034338cdd0800af650d17a))


## [0.3.0](https://github.com/gjtorikian/instagrab/compare/v0.2.0...v0.3.0) (2026-09-15)

### ⚠ BREAKING CHANGES

* `parse_web_profile_info`, `parse_user_feed`, and

### Features

* capture and replay IG's GraphQL queries ([#2](https://github.com/gjtorikian/instagrab/pull/2))

### Documentation

* **deploy:** add crates.io install and fix placeholders ([#2](https://github.com/gjtorikian/instagrab/pull/2))

### Miscellaneous Chores

* switch release to shared rust_crate_release workflow ([818f992](https://github.com/gjtorikian/instagrab/commit/818f992781678ac0dbed2b0948227dcbb9dda924))

