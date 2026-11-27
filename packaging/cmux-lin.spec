# Binary RPM of an already built cmux-lin tree (scripts/30-build.sh), made
# by scripts/50-package-rpm.sh, which passes the repo path as %{repo}.
Name:           cmux-lin
Version:        %{version_}
Release:        %{release_}%{?dist}
Summary:        Linux build of cmux, the terminal multiplexer for AI coding agents
License:        GPL-3.0-or-later AND MIT
URL:            https://github.com/ms4ndst/cmux-lin
ExclusiveArch:  x86_64
Requires:       python3
Requires:       gtk4
Requires:       vte291-gtk4
Recommends:     chromium
Recommends:     libsecret
Recommends:     gh

# Prebuilt Rust/Zig binaries: no debug package, keep them as built.
%global debug_package %{nil}
%global __strip /bin/true

%description
cmux-tui (upstream cmux's Rust multiplexer, with cmux-lin's patches), the
cmux-gtk native window, the cmux-browser-provider for browser tabs, the
cmux-theme switcher with its themes, and shell completions.

%prep

%build

%install
r=%{repo}
install -Dm0755 $r/build/cmux/cmux-tui/target/release/cmux-tui %{buildroot}%{_bindir}/cmux-tui
install -Dm0755 $r/build/cmux/cmux-tui/target/release/cmux-tui-hook %{buildroot}%{_bindir}/cmux-tui-hook
ln -s cmux-tui %{buildroot}%{_bindir}/cmux
install -Dm0755 $r/browser-provider/target/release/cmux-browser-provider %{buildroot}%{_bindir}/cmux-browser-provider
install -Dm0755 $r/gui/cmux-gtk/target/release/cmux-gtk %{buildroot}%{_bindir}/cmux-gtk
install -Dm0755 $r/scripts/cmux-theme.sh %{buildroot}%{_bindir}/cmux-theme
install -d %{buildroot}%{_datadir}/cmux-lin/themes
install -m0644 $r/assets/themes/*.json %{buildroot}%{_datadir}/cmux-lin/themes/
install -Dm0644 $r/assets/cmux-tui.json %{buildroot}%{_datadir}/cmux-lin/cmux-tui.json
install -Dm0755 $r/scripts/install-theme-picker.py %{buildroot}%{_datadir}/cmux-lin/install-theme-picker.py
install -Dm0644 /dev/stdin %{buildroot}%{_datadir}/applications/dev.cmuxlin.CmuxGtk.main.desktop <<DESKTOP
[Desktop Entry]
Type=Application
Name=cmux
GenericName=Terminal multiplexer
Comment=Native window for the cmux session "main"
Exec=%{_bindir}/cmux-gtk --session main
Icon=utilities-terminal
Terminal=false
Categories=System;TerminalEmulator;
Keywords=terminal;multiplexer;agents;tmux;
StartupWMClass=dev.cmuxlin.CmuxGtk.main
DESKTOP
for name in cmux cmux-tui cmux-theme cmux-gtk; do
  install -Dm0644 %{completions}/bash/cmux %{buildroot}%{_datadir}/bash-completion/completions/$name
  install -Dm0644 %{completions}/fish/$name.fish %{buildroot}%{_datadir}/fish/vendor_completions.d/$name.fish
done
install -Dm0644 %{completions}/zsh/_cmux %{buildroot}%{_datadir}/zsh/site-functions/_cmux
install -Dm0644 $r/LICENSE %{buildroot}%{_licensedir}/%{name}/LICENSE
install -Dm0644 $r/README.md %{buildroot}%{_docdir}/%{name}/README.md
install -Dm0644 $r/gui/cmux-gtk/README.md %{buildroot}%{_docdir}/%{name}/cmux-gtk.md

%files
%license %{_licensedir}/%{name}/LICENSE
%doc %{_docdir}/%{name}/README.md
%doc %{_docdir}/%{name}/cmux-gtk.md
%{_bindir}/cmux
%{_bindir}/cmux-tui
%{_bindir}/cmux-tui-hook
%{_bindir}/cmux-browser-provider
%{_bindir}/cmux-gtk
%{_bindir}/cmux-theme
%{_datadir}/cmux-lin/
%{_datadir}/applications/dev.cmuxlin.CmuxGtk.main.desktop
%{_datadir}/bash-completion/completions/cmux
%{_datadir}/bash-completion/completions/cmux-tui
%{_datadir}/bash-completion/completions/cmux-theme
%{_datadir}/bash-completion/completions/cmux-gtk
%{_datadir}/fish/vendor_completions.d/*.fish
%{_datadir}/zsh/site-functions/_cmux
