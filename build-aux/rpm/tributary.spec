Name:           tributary
Version:        v0.1.0
Release:        1.20260403103619142464.main.2.g5f3e5c5%{?dist}
Summary:        A high-performance media manager with unified local and remote backends

License:        GPL-3.0-or-later
URL:            https://github.com/jm2/tributary
Source0:        https://github.com/jm2/tributary/archive/%{version}.tar.gz#/%{name}-%{version}.tar.gz

BuildRequires:  rust
BuildRequires:  cargo
BuildRequires:  gcc
BuildRequires:  pkgconf-pkg-config
BuildRequires:  libadwaita-devel
BuildRequires:  gtk4-devel
BuildRequires:  pkgconfig(gtk4)
BuildRequires:  pkgconfig(libadwaita-1)
BuildRequires:  pkgconfig(gstreamer-1.0)
BuildRequires:  pkgconfig(dbus-1)
BuildRequires:  desktop-file-utils
BuildRequires:  libappstream-glib

Requires:       gtk4 >= 4.14
Requires:       libadwaita >= 1.5
Requires:       gstreamer1-plugins-good
Requires:       gstreamer1-plugins-bad-free
Requires:       gstreamer1-plugins-ugly-free
Requires:       gstreamer1-libav
Requires:       dbus

%description
Tributary is a high-performance, Rhythmbox-style media manager designed 
for GNOME. It features unified backends for local music and remote services 
like Subsonic, Jellyfin, and Plex.

%prep
%autosetup -p1 -n tributary-%{version}

%build
cargo build --offline --release

%install
# Install binary
install -D -p -m 0755 target/release/tributary %{buildroot}%{_bindir}/tributary

# Install desktop file
install -D -p -m 0644 data/io.github.tributary.Tributary.desktop %{buildroot}%{_datadir}/applications/io.github.tributary.Tributary.desktop

# Install metainfo
install -D -p -m 0644 data/io.github.tributary.Tributary.metainfo.xml %{buildroot}%{_metainfodir}/io.github.tributary.Tributary.metainfo.xml

# Install icons
for size in 16x16 24x24 32x32 48x48 64x64 128x128 256x256 512x512; do
    install -D -p -m 0644 data/icons/hicolor/${size}/apps/io.github.tributary.Tributary.png \
        %{buildroot}%{_datadir}/icons/hicolor/${size}/apps/io.github.tributary.Tributary.png
done

%check
desktop-file-validate %{buildroot}%{_datadir}/applications/*.desktop
appstream-util validate-relax --nonet %{buildroot}%{_metainfodir}/*.metainfo.xml

%files
%license LICENSE
%doc README.md
%{_bindir}/tributary
%{_datadir}/applications/io.github.tributary.Tributary.desktop
%{_metainfodir}/io.github.tributary.Tributary.metainfo.xml
%{_datadir}/icons/hicolor/*/apps/io.github.tributary.Tributary.png

%changelog
* Fri Apr 03 2026 John-Michael Mulesa <jmulesa@gmail.com> - v0.1.0-1.20260403103619142464.main.2.g5f3e5c5
- feat: add full RPM packaging support and Packit configuration for Fedora COPR builds (John-Michael Mulesa)
- chore: disable LTO in PKGBUILD and remove redundant makepkg configuration overrides (John-Michael Mulesa)

* Fri Apr 03 2026 Tributary Contributors <tributary@example.com> - 0.1.0-1
- Initial Fedora package
