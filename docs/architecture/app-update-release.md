# Build evidence for signed DevWorks app updates

Use a reviewed source revision and the ordinary DevWorks feature and PR workflow to produce a candidate. The source revision, build, merged commit and installed application are separate facts. Record the candidate build and review on the work item. The app activates a released build only after the local user confirms the exact signed stage.

## Configure a release build

Set `DEVWORKS_RELEASE_BUILD` to the full lowercase Git object ID and `DEVWORKS_RELEASE_PUBLIC_KEYS` to the comma-separated, lowercase hex Ed25519 public keys at build time. The release authority signs the exact manifest bytes. Keep its private keys in the release signing system. An older installed app must already trust a key that signs an update that introduces new keys.

Set `DEVWORKS_SIGNING_TEAM` for a macOS Developer ID release or `DEVWORKS_SIGNING_CERT_SHA256` for a Windows Authenticode release. The macOS application identifier is `com.sceneworks.devworks`. The Windows pin is SHA-256 of the signer certificate’s DER encoding. Validate platform signing, install locations, ownership and offline operation for each target before advertising it.

The app recognizes a Windows per-user installation in LocalAppData with an unchanged signed `DevWorks.exe` launcher and a `current` file containing the selected content-addressed version directory under `versions`. The macOS per-user bundle lives in the user's Applications directory. The Linux standalone install lives under the user's `.local/opt/devworks`. macOS and Linux require an installation volume that supports atomic directory exchange. Distribution through a system package manager uses that manager's transaction.

## Package the exact installed image

Run the built desktop executable with `--release-evidence` to read the compiled version, source revision, target, bundled Help digest and ordered schema migration witnesses. Use the OS-signed bytes in the bundle. Include every regular file in the signed inventory. Each file carries its path, executable flag, byte length and lowercase SHA-256 digest. Sort entries by path and concatenate their exact bytes in that order to make the archive. The archive's byte length and SHA-256 digest are independently committed. Relative paths use ordinary ASCII names and a slash separator.

Use the corresponding `DevWorks-<target>.manifest.json`, `DevWorks-<target>.manifest.sig` and `DevWorks-<target>.bundle` assets on the published `v<version>` release. The format 1 manifest binds the product `SceneWorks/DevWorks`, release ID, tag, source build, target, owner, archive identity, Help digest and exact migration witnesses. The signature asset is the raw 64-byte Ed25519 signature over the manifest file. The native manifest record and its validation live in `crates/devworks-core/src/ports/update.rs`.

Prepare a reviewed, user-initiated workspace migration for any irreversible authored-data change. The updater refuses declared destructive migrations in the automatic startup path. Qualify install and rollback on isolated profiles with authored data, held work, expected Help and matching binaries before distribution. Keep selected signing credentials and production install targets in the release qualification record.
