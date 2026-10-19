# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0](https://github.com/schnedo/imapmaildir/compare/v0.1.15...v0.2.0)

### Features


- Allow resuming interrupted initial sync - ([c3fcf49](https://github.com/schnedo/imapmaildir/commit/c3fcf49c1a23a8866a75b1e5c67fd8b8e36cb5fc) (by @schnedo))
- Cleanly logout at and of sync_once - ([9a6ac27](https://github.com/schnedo/imapmaildir/commit/9a6ac2742bbc4d498311852b87a6689011eb10b0) (by @schnedo))
- Migrate from tokio to compio - ([f715c17](https://github.com/schnedo/imapmaildir/commit/f715c17002d699f27f05e19b75910d19cd9ec39f) (by @schnedo))

### Bug Fixes


- *(deps)* Update rust crate crossfire to 3.1.21 - ([7228149](https://github.com/schnedo/imapmaildir/commit/72281493c8258caf9b5e6c03ae5677c832e5d38e) (by @renovate[bot]))
- Expect at least one mail when storing and skip storing without new local mail - ([3e33fd4](https://github.com/schnedo/imapmaildir/commit/3e33fd4d24feb83485f6fc68c4bec9d7ff8737ec) (by @schnedo))
- Accept confirmation of nuke command - ([e320c41](https://github.com/schnedo/imapmaildir/commit/e320c41296511e337c3db1beb54c4ee9312330dc) (by @schnedo))
- Report actual error when loading existing maildir repository fails - ([2882952](https://github.com/schnedo/imapmaildir/commit/28829527f942b41e05e1d350a40db57479449b08) (by @schnedo))
- Use per mail modseq for conditional flag removal after flag addition - ([40599c9](https://github.com/schnedo/imapmaildir/commit/40599c99494c365fec027e84f5d091c7a47fe577) (by @schnedo))
- Do not include whole receive buffer in response parsing errors - ([056e3a4](https://github.com/schnedo/imapmaildir/commit/056e3a499de51e776aad54df8a07fc6090066538) (by @schnedo))
- Only call on_change hook if remote updates changed local mail - ([9a4f65c](https://github.com/schnedo/imapmaildir/commit/9a4f65ca35860477d3117c7d21e273bf7e91756d) (by @schnedo))
- Wait for pending on_change hook before exiting sync once - ([0f4ad54](https://github.com/schnedo/imapmaildir/commit/0f4ad54459249cd3d7b36fdd4696dfe2aee14fcf) (by @schnedo))
- Do not keep ignoring files after failed or skipped file operations - ([7e96002](https://github.com/schnedo/imapmaildir/commit/7e9600285a9e260c3dd4de2542c4fa5c1dbcbe44) (by @schnedo))
- Poll for changes if server does not support or rejects IDLE - ([c756ab7](https://github.com/schnedo/imapmaildir/commit/c756ab78a58fcdd3d215f0c3aaddc10b9a2e7fa1) (by @schnedo))
- Merge multiple VANISHED responses during select - ([4577703](https://github.com/schnedo/imapmaildir/commit/4577703c47c353d0ead23d4d7c90429b39bcb02e) (by @schnedo))
- Sync local mail delivered to new by moving it into cur - ([8055af4](https://github.com/schnedo/imapmaildir/commit/8055af4f7ffe534973d29a2f9230889e726a292f) (by @schnedo))
- Parse FETCH responses independent of attribute order - ([402c578](https://github.com/schnedo/imapmaildir/commit/402c57851f3fb0bd4a360d4c9a19853993d043dc) (by @schnedo))
- Ignore removal of duplicate mail instead of its kept twin during rename - ([484213a](https://github.com/schnedo/imapmaildir/commit/484213a750aa49c00ffa58104d58dff5f6aff859) (by @schnedo))
- Ignore directories in cur and rescan maildir on inotify queue overflow - ([fee3697](https://github.com/schnedo/imapmaildir/commit/fee3697d427d88a972e3d1d231586f2aa837c0b8) (by @schnedo))
- Wait for new local mail to be written completely before syncing it - ([c2b801b](https://github.com/schnedo/imapmaildir/commit/c2b801b7d8b15815bea979ac8d75ca0f7c16d4b1) (by @schnedo))
- Quote command arguments and encode mailbox names as modified UTF-7 - ([e262f66](https://github.com/schnedo/imapmaildir/commit/e262f66b3a2a2097681d1bfe48b0a2197ed7538c) (by @schnedo))
- Append mails one by one if server lacks MULTIAPPEND and require IDLE for idling - ([2c827cb](https://github.com/schnedo/imapmaildir/commit/2c827cb2cec3862a19038f564a3f06a37abe53fa) (by @schnedo))
- Retry or reconcile local changes rejected by server - ([2a294bb](https://github.com/schnedo/imapmaildir/commit/2a294bb9bae1173a408ee7f1e68ae6fd73c65f3c) (by @schnedo))
- Exit on panic in idle mode instead of continuing with dead sync tasks - ([49ec3db](https://github.com/schnedo/imapmaildir/commit/49ec3dbce5358ec7dc4ef1747c38be25dec077c7) (by @schnedo))
- Prevent duplicate upload after crash by committing state before moving mail into cur - ([7f5cefd](https://github.com/schnedo/imapmaildir/commit/7f5cefde38390aa3f0420dbdec214936e11e3552) (by @schnedo))
- Fetch only metadata for remote changes after idle and download just missing mail - ([2c353bd](https://github.com/schnedo/imapmaildir/commit/2c353bdb229433c77c1f2fb8bd2b4025d4cfef76) (by @schnedo))
- Do not crash on remote flag update of locally missing mail - ([becc090](https://github.com/schnedo/imapmaildir/commit/becc090244a9f29f31d034af14632f5fbf87eb2d) (by @schnedo))
- Do not reparse already parsed data with incomplete received mail - ([b4d2113](https://github.com/schnedo/imapmaildir/commit/b4d2113899b050eaed0812d80a984b14c72c1abd) (by @schnedo))
- Correctly sync mail arriving outside of idle - ([d99d039](https://github.com/schnedo/imapmaildir/commit/d99d039412b7a2bd2044ce5fa2d7b0b5ed8e13a7) (by @schnedo))
- Prevent lost remote update on sync interrupt by centralized highest modseq handling - ([3baaf7b](https://github.com/schnedo/imapmaildir/commit/3baaf7b91a7d4f5dc3d209ec2aeb05abfbc3910f) (by @schnedo))

### Performance


- Avoid per mail allocations when appending mails - ([f121dd3](https://github.com/schnedo/imapmaildir/commit/f121dd35b3d04594d7837086c10e15e028f9c9e2) (by @schnedo))
- Increase release profile optimization level - ([6b01c99](https://github.com/schnedo/imapmaildir/commit/6b01c99f93571093bdd9aa73be603c5104980e70) (by @schnedo))
- Batch applying remote changes to local - ([f6c06de](https://github.com/schnedo/imapmaildir/commit/f6c06dedc47b6c2c72345515a8b83ff34c16b374) (by @schnedo))
- Batch up local changes - ([3e24920](https://github.com/schnedo/imapmaildir/commit/3e2492077527303c4e6a1eec95ccd6a2eceaa125) (by @schnedo))
- Batch writing new received mail - ([f10d4e2](https://github.com/schnedo/imapmaildir/commit/f10d4e245d54bd33e00727c7cd4635b6ccbf205d) (by @schnedo))
- Cache PID and hostname for building mail file prefix - ([d81f070](https://github.com/schnedo/imapmaildir/commit/d81f070926789e4a5f3f546001e7197201203800) (by @schnedo))

### Chore


- *(deps)* Lock file maintenance - ([975fe53](https://github.com/schnedo/imapmaildir/commit/975fe531bd20734656514cd60c3733f3a57ee1ab) (by @renovate[bot]))
- *(deps)* Update rust crate rstest to v0.28.0 - ([c94855d](https://github.com/schnedo/imapmaildir/commit/c94855d41b72a956e0ee37cf9df440e47f40c6bc) (by @renovate[bot]))
- Remove benchmarks - ([75cb7c1](https://github.com/schnedo/imapmaildir/commit/75cb7c1b144218e42d24d4a5a4dd47ab69a06fa0) (by @schnedo))

### Refactor


- Require IDLE for continuous sync instead of polling - ([b2a542c](https://github.com/schnedo/imapmaildir/commit/b2a542c29b5c98d416cf30d0c7e48b0601002b05) (by @schnedo))
- Increase tcp buffer size - ([9694d3c](https://github.com/schnedo/imapmaildir/commit/9694d3cf0b67cf403acae2d90389eed3eb3bfd4f) (by @schnedo))
- Run more tasks concurrently - ([5ea521e](https://github.com/schnedo/imapmaildir/commit/5ea521e5b3f82d20412b680a558bf194f71934df) (by @schnedo))
- Use channels instead of cloning maildir_repository - ([3e51dc1](https://github.com/schnedo/imapmaildir/commit/3e51dc19848a699c2e434763f89f041f785b7533) (by @schnedo))
- Use more fitting channel types when applicable - ([f6dcb3b](https://github.com/schnedo/imapmaildir/commit/f6dcb3b1d151210b768e0cda24e554978cc1439d) (by @schnedo))
- Setup tls connector only once for all mailboxes - ([6a91abd](https://github.com/schnedo/imapmaildir/commit/6a91abd026bb521d1d837a2c65dc2b09f63140ee) (by @schnedo))
- Execute password cmd asynchronously - ([41a9e9a](https://github.com/schnedo/imapmaildir/commit/41a9e9a56fcce179b3a2325bbaead27befd9e00c) (by @schnedo))
- Offload sync db transactions into separate thread - ([49af277](https://github.com/schnedo/imapmaildir/commit/49af277479be9a94cd0499e5ee06f927f974c0b8) (by @schnedo))
- Use compio::fs for file operations - ([1f30f90](https://github.com/schnedo/imapmaildir/commit/1f30f90ab76b733e9edc00d32034535a8a9db771) (by @schnedo))
- Use command specific handlers for handling responses alongside general handling - ([e03a436](https://github.com/schnedo/imapmaildir/commit/e03a4367f0e15d0c3509dfeb88160cf2e1be8531) (by @schnedo))

### Contributors

* @renovate[bot]
* @schnedo

## [0.1.15](https://github.com/schnedo/imapmaildir/compare/v0.1.14...v0.1.15)

### Bug Fixes


- *(deps)* Update rust crate tokio to 1.53.3 - ([f534f9e](https://github.com/schnedo/imapmaildir/commit/f534f9eda1fdce429c6c9ef6c45f5c9079119047) (by @renovate[bot]))

### Contributors

* @renovate[bot]

## [0.1.14](https://github.com/schnedo/imapmaildir/compare/v0.1.13...v0.1.14)

### Bug Fixes


- *(deps)* Update rust-futures monorepo to 0.3.34 - ([7030762](https://github.com/schnedo/imapmaildir/commit/703076207106cd02096187d6346c5a2aa7df9a0d) (by @renovate[bot]))
- *(deps)* Update rust crate toml to 1.1.8 - ([dbfb51e](https://github.com/schnedo/imapmaildir/commit/dbfb51ec6bd8b4feb76e50c3ce1a9b1ecb559b63) (by @renovate[bot]))

### Contributors

* @renovate[bot]

## [0.1.13](https://github.com/schnedo/imapmaildir/compare/v0.1.12...v0.1.13)

### Bug Fixes


- *(deps)* Update rust crate tokio-util to 0.7.20 - ([98862b7](https://github.com/schnedo/imapmaildir/commit/98862b7ab4cb2ac0b7d5f01fc03e5323835c59dd) (by @renovate[bot]))

### Contributors

* @renovate[bot]

## [0.1.12](https://github.com/schnedo/imapmaildir/compare/v0.1.11...v0.1.12)

### Bug Fixes


- *(deps)* Update rust crate tokio to 1.53.2 - ([39fb209](https://github.com/schnedo/imapmaildir/commit/39fb2090a3523ae30a9b133aa23eb8941beda03e) (by @renovate[bot]))
- *(deps)* Update rust crate thiserror to 2.0.21 - ([9fe9b88](https://github.com/schnedo/imapmaildir/commit/9fe9b88dc79c798a73f49c81b3a7cf8547c35ea0) (by @renovate[bot]))

### Contributors

* @renovate[bot]

## [0.1.11](https://github.com/schnedo/imapmaildir/compare/v0.1.10...v0.1.11)

### Bug Fixes


- *(deps)* Update rust crate serde to 1.0.229 - ([b670905](https://github.com/schnedo/imapmaildir/commit/b67090550bea5e2578a6c437129065ed8e6dec52) (by @renovate[bot]))

### Contributors

* @renovate[bot]

## [0.1.10](https://github.com/schnedo/imapmaildir/compare/v0.1.9...v0.1.10)

### Bug Fixes


- *(deps)* Update rust crate rustix to 1.1.5 - ([4bea182](https://github.com/schnedo/imapmaildir/commit/4bea182906807b7c4d9b65d0b8b189c54c79782f) (by @renovate[bot]))

### Contributors

* @renovate[bot]

## [0.1.9](https://github.com/schnedo/imapmaildir/compare/v0.1.8...v0.1.9)

### Bug Fixes


- *(deps)* Update rust crate rusqlite_migration to 2.6.0 - ([6d251d4](https://github.com/schnedo/imapmaildir/commit/6d251d4c6fa56e7a0d4a3ce9083247b9bbcfeb13) (by @renovate[bot]))

### Contributors

* @renovate[bot]

## [0.1.8](https://github.com/schnedo/imapmaildir/compare/v0.1.7...v0.1.8)

### Bug Fixes


- *(deps)* Update rust crate rusqlite to 0.40.2 - ([4ccd6a2](https://github.com/schnedo/imapmaildir/commit/4ccd6a243787f473750222c72be71dcafc4aea6d) (by @renovate[bot]))
- *(deps)* Update rust crate log to 0.4.34 - ([acab4de](https://github.com/schnedo/imapmaildir/commit/acab4def100d786d9669444eaf0eea978db0c89c) (by @renovate[bot]))

### Chore


- Only bench runtime things - ([d270382](https://github.com/schnedo/imapmaildir/commit/d270382bc6b2e98a50a61c4bf7ca1e5f72f40e7a) (by @schnedo))

### Contributors

* @renovate[bot]
* @schnedo

## [0.1.7](https://github.com/schnedo/imapmaildir/compare/v0.1.6...v0.1.7)

### Bug Fixes


- *(deps)* Update rust crate jiff to 0.2.38 - ([3e4eecf](https://github.com/schnedo/imapmaildir/commit/3e4eecf502effa6134d42de43208cf302841edb8) (by @renovate[bot]))

### Contributors

* @renovate[bot]

## [0.1.6](https://github.com/schnedo/imapmaildir/compare/v0.1.5...v0.1.6)

### Bug Fixes


- *(deps)* Update rust crate inotify to 0.11.5 - ([0fe06a4](https://github.com/schnedo/imapmaildir/commit/0fe06a4086456a53f3ad3fba9486f5ac6070cb24) (by @renovate[bot]))

### Chore


- Add performance tests - ([1b1865b](https://github.com/schnedo/imapmaildir/commit/1b1865bba14c01c99217f015293c00ed1ab31c24) (by @schnedo))

### Contributors

* @schnedo
* @renovate[bot]

## [0.1.5](https://github.com/schnedo/imapmaildir/compare/v0.1.4...v0.1.5)

### Bug Fixes


- *(deps)* Update rust crate env_logger to 0.11.11 - ([a25e4b4](https://github.com/schnedo/imapmaildir/commit/a25e4b4fd224b05720fa808361121835761822c0) (by @renovate[bot]))

### Contributors

* @renovate[bot]

## [0.1.4](https://github.com/schnedo/imapmaildir/compare/v0.1.3...v0.1.4)

### Bug Fixes


- *(deps)* Update rust crate clap to 4.6.7 - ([ca48778](https://github.com/schnedo/imapmaildir/commit/ca487786f1c9cc04284d26fedb82a992c7400dcf) (by @renovate[bot]))

### Contributors

* @renovate[bot]

## [0.1.3](https://github.com/schnedo/imapmaildir/compare/v0.1.2...v0.1.3)

### Bug Fixes


- *(deps)* Update rust crate bytes to 1.12.1 - ([9a5cf48](https://github.com/schnedo/imapmaildir/commit/9a5cf483205a9875bd4853d35e8aabaa325f6418) (by @renovate[bot]))

### Contributors

* @renovate[bot]

## [0.1.2](https://github.com/schnedo/imapmaildir/compare/v0.1.1...v0.1.2)

### Bug Fixes


- *(deps)* Update rust crate anyhow to 1.0.104 - ([2fb0ae0](https://github.com/schnedo/imapmaildir/commit/2fb0ae01174fe9ceceebefcdf8da17f47393ab9b) (by @renovate[bot]))
- *(deps)* Update rust crate anstyle to 1.0.14 - ([07398e7](https://github.com/schnedo/imapmaildir/commit/07398e7d11d278b434dc16ad1aad4548c604a083) (by @renovate[bot]))

### Contributors

* @renovate[bot]

## [0.1.1](https://github.com/schnedo/imapmaildir/compare/v0.1.0...v0.1.1)

### Bug Fixes


- *(deps)* Update rust crate imap-proto to 0.17.0 - ([8fa839d](https://github.com/schnedo/imapmaildir/commit/8fa839d27c53ff872d5747ea5f55bb81934e19f3) (by @renovate[bot]))

### Chore


- *(deps)* Update rust crate toml to v1.1.7 - ([9fdc8c8](https://github.com/schnedo/imapmaildir/commit/9fdc8c8fbfb568ee3386ff463ceb20de94627780) (by @renovate[bot]))
- *(deps)* Update rust crate tokio to v1.53.2 - ([f9c10b0](https://github.com/schnedo/imapmaildir/commit/f9c10b025dc18cf36e4371779bdde5227d1a6e8c) (by @renovate[bot]))
- *(deps)* Update rust crate jiff to v0.2.38 - ([9938bfb](https://github.com/schnedo/imapmaildir/commit/9938bfbf981aaa1c75759f25cf9a0722036adc1b) (by @renovate[bot]))
- *(deps)* Lock file maintenance - ([d5bb670](https://github.com/schnedo/imapmaildir/commit/d5bb670507ffb0e83e5337e79d24df29e3e1937c) (by @renovate[bot]))
- *(deps)* Update rust crate jiff to v0.2.37 - ([32bd948](https://github.com/schnedo/imapmaildir/commit/32bd94856a7419d0c70ea7a628314a7f5e6ede73) (by @renovate[bot]))
- *(deps)* Update rust crate toml to v1.1.6 - ([9a631e2](https://github.com/schnedo/imapmaildir/commit/9a631e2069d461c9fda0312c58962afb2d386548) (by @renovate[bot]))
- *(deps)* Update rust crate rstest to v0.27.0 - ([4db5717](https://github.com/schnedo/imapmaildir/commit/4db57175f2951b408ba106ef0ac85c015f766eec) (by @renovate[bot]))
- *(deps)* Update rust crate toml to v1.1.5 - ([ef4721f](https://github.com/schnedo/imapmaildir/commit/ef4721fae2f043179e492b2e45436839ee3bd02e) (by @renovate[bot]))
- *(deps)* Update rust crate log to v0.4.34 - ([76e1630](https://github.com/schnedo/imapmaildir/commit/76e16309fb66e1fb55655f1534d7ed2e4b22cb64) (by @renovate[bot]))
- *(deps)* Update rust crate inotify to v0.11.5 - ([8b35990](https://github.com/schnedo/imapmaildir/commit/8b35990cf958d7fc0ff29553686ea77ffbcb9d5f) (by @renovate[bot]))
- *(deps)* Update rust-futures monorepo to v0.3.34 - ([0d7fba3](https://github.com/schnedo/imapmaildir/commit/0d7fba3a9b988b509b968fa1bad051ae303ec222) (by @renovate[bot]))
- *(deps)* Update rust crate thiserror to v2.0.20 - ([4895bc9](https://github.com/schnedo/imapmaildir/commit/4895bc9a934f5ee40f73baf08c3dccad743afd94) (by @renovate[bot]))
- *(deps)* Update rust crate rusqlite to v0.40.2 - ([4bbea44](https://github.com/schnedo/imapmaildir/commit/4bbea44555faffef0b1ac84b5b0f2c9392a29848) (by @renovate[bot]))
- *(deps)* Update rust crate clap to v4.6.6 - ([b9f84a9](https://github.com/schnedo/imapmaildir/commit/b9f84a901a978916fe57bc082796be2c9e2df434) (by @renovate[bot]))
- *(deps)* Update rust crate testcontainers to v0.28.0 - ([b92ce2e](https://github.com/schnedo/imapmaildir/commit/b92ce2ee378cb70d4502df1d7f12ac263ce690d7) (by @renovate[bot]))
- *(deps)* Update rust crate toml to v1.1.4 - ([7dfdce7](https://github.com/schnedo/imapmaildir/commit/7dfdce7b38389f04b631a91753897c0f32c43605) (by @renovate[bot]))
- *(deps)* Update rust crate clap to v4.6.5 - ([4338db0](https://github.com/schnedo/imapmaildir/commit/4338db0bf88c41421b9c942080922cd92ec670a3) (by @renovate[bot]))
- *(deps)* Remove redundand & - ([42f9db7](https://github.com/schnedo/imapmaildir/commit/42f9db790df92b7251b69e77e73c951672ab0996) (by @schnedo))
- *(deps)* Update rust crate jiff to v0.2.35 - ([5cf18fa](https://github.com/schnedo/imapmaildir/commit/5cf18fa462adecea2bebdb2837f322413de56090) (by @renovate[bot]))
- *(deps)* Update rust crate tokio-util to v0.7.19 - ([27e2fe3](https://github.com/schnedo/imapmaildir/commit/27e2fe350f59a5ce4df2ff602a664aa947afaab9) (by @renovate[bot]))
- *(deps)* Update rust crate tokio to v1.53.1 - ([c157e8c](https://github.com/schnedo/imapmaildir/commit/c157e8cfe21eae9430c5e62214ef410d0a1035c9) (by @renovate[bot]))
- *(deps)* Update rust crate clap to v4.6.3 - ([673d60e](https://github.com/schnedo/imapmaildir/commit/673d60ee9ec8ab91f35a157991c5071c44f3fedb) (by @renovate[bot]))
- *(deps)* Update rust crate jiff to v0.2.34 - ([b215740](https://github.com/schnedo/imapmaildir/commit/b2157404725c5a5b0cf9aa4b8a1525f351b7ce95) (by @renovate[bot]))
- *(deps)* Update rust crate thiserror to v2.0.19 - ([7d1816e](https://github.com/schnedo/imapmaildir/commit/7d1816effbc33cf378339665d9aef3bf7557d47a) (by @renovate[bot]))
- *(deps)* Update rust crate serde to v1.0.229 - ([e51537b](https://github.com/schnedo/imapmaildir/commit/e51537b8aabf74547f262b1182051b380dcdc8fa) (by @renovate[bot]))
- *(deps)* Update rust crate jiff to v0.2.33 - ([e971abb](https://github.com/schnedo/imapmaildir/commit/e971abbed871305c5f104b2bfe268d3f5d9a839c) (by @renovate[bot]))
- *(deps)* Update rust crate anyhow to v1.0.104 - ([c107ab1](https://github.com/schnedo/imapmaildir/commit/c107ab13496ff517ebf663c0f66c347e029aa35e) (by @renovate[bot]))
- *(deps)* Update rust-futures monorepo to v0.3.33 - ([66618ad](https://github.com/schnedo/imapmaildir/commit/66618ad86a67963f2a6530bb8aa32449d41f3588) (by @renovate[bot]))
- *(deps)* Update rust crate tokio to v1.53.0 - ([f680662](https://github.com/schnedo/imapmaildir/commit/f6806626c2ef32551cee99a1a269f23a46730a9f) (by @renovate[bot]))
- *(deps)* Update rust crate clap to v4.6.2 - ([5265c7b](https://github.com/schnedo/imapmaildir/commit/5265c7b4d2bf2c694b3d0b591c7b3729a8ac2c46) (by @renovate[bot]))
- *(deps)* Update rust crate toml to v1.1.3 - ([47a6b3d](https://github.com/schnedo/imapmaildir/commit/47a6b3ddb6f90896345cb0cb083b6ae60c3282e3) (by @renovate[bot]))
- Adhere to updated clippy rules - ([e0c646b](https://github.com/schnedo/imapmaildir/commit/e0c646bd8a908753a8e631eeae48500ab325aff2) (by @schnedo))

### Contributors

* @renovate[bot]
* @schnedo
