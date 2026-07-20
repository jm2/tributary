# Release component policy

Last reviewed: 2026-07-20

Tributary is a music-library application. It does not implement DVD, Blu-ray, DRM-protected-media,
or proprietary content-decryption-module playback, so release artifacts must not contain dedicated
copy-control circumvention components or the unused optical-disc access plugins that can introduce
them transitively.

This is a conservative release-engineering boundary, not a representation that a filename by
itself determines a component's legal status and not legal advice. Laws, licenses, patents, and
distribution rules vary by jurisdiction. A release owner must still review any new media stack,
codec, protocol, or packaging source on its own facts.

## Denied release components

[`build-aux/packaging/forbidden-bundled-components.txt`](../build-aux/packaging/forbidden-bundled-components.txt)
is the machine-readable, case-insensitive filename-token policy used by every packaging helper.
It covers:

- dedicated CSS, AACS, BD+, and MakeMKV-style optical-disc decryption bridges;
- dedicated DVD access libraries and unused DVD/Blu-ray GStreamer plugins that Tributary does not
  use; these are excluded conservatively even when a particular library does not itself decrypt
  media;
- standalone MakeMKV tooling, the dedicated Debian `libdvd-pkg` installer, and conventional AACS
  key-database artifacts; and
- proprietary browser/video content-decryption modules such as Widevine, PlayReady, or FairPlay.

The policy intentionally does not deny ordinary audio decoders, container parsers, TLS libraries,
or general-purpose cryptography. Those components support Tributary's normal authorized playback
and transport security and are distinct from a dedicated copy-control bypass facility. Their
licenses and any applicable codec-patent obligations remain a separate distribution review.
VideoLAN states that `libbluray` itself contains no DRM-circumvention tool; MSYS2's ordinary
FFmpeg/libav build links it transitively for Blu-ray access/navigation, so the generic library is
allowed while the unused `gstbluray` playback plugin and the separate AACS/BD+ decrypt libraries
remain denied.

## Enforcement

Packaging fails closed when the policy file is missing, malformed, or empty. Platform helpers then
apply the policy at the boundaries available to them:

- Windows and macOS omit denied GStreamer plugins before native dependency traversal, reject a
  denied dependency discovered during that traversal, and recursively scan the finished portable
  application before archive, installer, signing, or disk-image creation. Windows rejects source
  and destination filesystem reparse points before bundle writes, performs a bounded final import
  pass over every hidden-inclusive DLL/EXE, reopens the completed ZIP to validate every entry name,
  and makes installer-only mode repeat both the tree and import gates so a stale incremental bundle
  cannot bypass them. macOS release builds pin the repository policy and system inspection tools
  instead of honoring the helper's test hooks; native plugin/dependency source paths and linked
  dependency, rpath, and load-command paths are checked component by component under a fixed ASCII
  locale, and Mach-O magic triggers import inspection even without an executable bit or conventional
  name.
- Native Linux packages contain Tributary's payload rather than copies of distribution-provided
  GStreamer libraries. The helper checks Debian control data and maintainer scripts, RPM strong and
  weak relationships and scriptlets, Arch `.INSTALL` maintainer scripts, every bracket-valued ELF
  dynamic-section reference plus the program interpreter, and each completed `.deb`, `.rpm`, and
  Arch payload. ELF inspection uses GNU `readelf` because the elfutils variant does not resolve all
  filtered-library names, and uses a private, failure-cleaned workspace. The RPM build recipe also
  validates its installed buildroot, and the Arch recipe validates its installed tree even when
  `check()` is skipped. This boundary does not treat the contents of separately resolved
  distribution packages as bundled in Tributary's package and does not attest those repositories.
- Flatpak validation scans the complete application commit: the `/app` payload and its ELF
  dependencies, app-owned exports, and application metadata. The separately delivered, shared
  Freedesktop/GNOME runtime is outside Tributary's bundle and outside that payload claim.

Repository tests pin the shared-policy use and fail-closed placement in all three helpers, including
hostile macOS test-hook values and Windows PowerShell 5.1 parsing. Release jobs run those helpers or
the same validators directly. Windows ZIP, native Linux package, and Flatpak outputs are reopened
and inspected before upload; the macOS app and Windows installer source tree are inspected
immediately before their trusted container tools run.

## Review boundary

Any change that weakens the denied list, adds a new bundled media framework/plugin source, enables
optical-disc or DRM-protected-media playback, or adds a content-decryption module requires a dedicated
design and distribution review. Do not add a silent exception to one platform script. Update the
shared policy, this document, tests, and changelog together, with the reason and artifact evidence
recorded in review.

The same review applies to a sender implementation that embeds protocol key material: a key being
public rather than private does not establish that its provenance or distribution is appropriate.
Normal authenticated transport encryption does not require a policy exception.

The 2026-07-20 sender review found that current official GStreamer, Homebrew, and MSYS2 packages do
not ship the `raopsink` element Tributary had described as an AirPlay 1 dependency. GStreamer's
historical, differently named `apexsink` was
[removed after remaining unported](https://github.com/GStreamer/gst-plugins-bad/commit/9b5de053995488d5ddc78c1bf4df651101271d70);
its [legacy implementation](https://github.com/GStreamer/gst-plugins-bad/blob/1.10.4/ext/apexsink/gstapexraop.c)
embedded only an RSA public modulus/exponent used to encrypt a generated outbound session key, not
a private key or DRM-protected-media decryptor. That distinction is useful
provenance evidence, not a legal conclusion and not a maintained sender path. Tributary therefore
does not bundle that legacy implementation and no longer tells users that `gst-plugins-bad`
provides `raopsink`. See the current [GStreamer element index](https://gstreamer.freedesktop.org/documentation/plugins_doc.html),
[MSYS2 package](https://packages.msys2.org/packages/mingw-w64-clang-x86_64-gst-plugins-bad?repo=clang64),
and [Homebrew formula](https://formulae.brew.sh/formula/gstreamer) for the supported dependency
boundary.

The gates detect accidental inclusion under recognizable component filenames and declared native
dependencies; they are not semantic malware scanners and cannot prove the behavior of an
arbitrarily renamed binary. They also do not recursively unpack an innocuously named nested
archive; Tributary's install manifests intentionally introduce no such container. Adding one is a
review-boundary change. Release inputs must therefore continue to come from the pinned or
documented package sources used by the build workflows.

A capability-derived audio-plugin allowlist and narrower native distribution relationships would
be stronger than recognizable-name denial, but they are not safe to guess from one build host:
GStreamer autoplugging, platform sinks, remote-source handling, supported containers, and any
future selected AirPlay sender must all remain covered. Some distributions place unused
disc-access plugins in the same broad plugin packages as supported audio capabilities; that does
not place them in Tributary's package payload, but it is a valid least-privilege follow-up. Treat
both changes as a separate cross-platform packaging improvement with a real playback matrix, not
as an unreviewed tightening of this emergency gate.

## Reference boundary

The conservative classification is informed by the official project descriptions for
[libdvdcss](https://images.videolan.org/developers/libdvdcss.html),
[libbluray](https://www.videolan.org/developers/libbluray.html),
[libaacs](https://images.videolan.org/developers/libaacs.html), and
[libbdplus](https://images.videolan.org/developers/libbdplus.html), together with GStreamer's own
[distribution and licensing guidance](https://gstreamer.freedesktop.org/documentation/frequently-asked-questions/licensing.html).
Those upstream pages make different claims about their projects and do not establish Tributary's
legal obligations. They are provenance for why an audio-only application takes the simpler course
of omitting dedicated decryptors and unused optical-disc playback plugins. Debian's
[`libdvd-pkg` documentation](https://sources.debian.org/data/contrib/libd/libdvd-pkg/1.6.0-1-1/debian/README.Debian)
is the provenance for denying that dedicated installer relationship even when its package name
does not contain `dvdcss`. For U.S. releases, the relevant statutory text is
[17 U.S.C. § 1201](https://uscode.house.gov/view.xhtml?req=%28title%3A17+section%3A1201+edition%3Aprelim%29);
release owners should obtain qualified advice when a future feature changes this boundary.
