Name:           shared-control
Version:        0.1.0
Release:        6%{?dist}
Summary:        Share mouse, keyboard and clipboard between two computers over USB radio dongles
License:        MIT OR Apache-2.0
URL:            https://example.invalid/shared-control
Source0:        %{name}-%{version}.tar.gz

%global debug_package %{nil}

%description
Shared Control links two computers through nRF52 USB dongles. The mouse,
keyboard and (optionally) clipboard of the master are used on the node.
Requires a Wayland session with the KDE RemoteDesktop and InputCapture portals.
Install -master or -node on each computer.

%package master
Summary:        Shared Control master (computer with mouse and keyboard)
Requires:       wl-clipboard
Requires:       kdialog
Requires:       systemd-libs

%description master
Runs on the computer whose mouse and keyboard control the node.

%package node
Summary:        Shared Control node (computer controlled by the master)
Requires:       wl-clipboard
Requires:       kdialog
Requires:       systemd-libs

%description node
Runs on the computer that receives mouse and keyboard input from the master.

%prep
%setup -q

%install
install -Dm755 shared-control-master %{buildroot}%{_bindir}/shared-control-master
install -Dm755 shared-control-node %{buildroot}%{_bindir}/shared-control-node
install -Dm644 60-shared-control.rules %{buildroot}%{_udevrulesdir}/60-shared-control.rules
install -Dm644 98-shared-control-master.rules %{buildroot}%{_udevrulesdir}/98-shared-control-master.rules
install -Dm644 98-shared-control-node.rules %{buildroot}%{_udevrulesdir}/98-shared-control-node.rules
install -Dm644 shared-control-master.service %{buildroot}%{_userunitdir}/shared-control-master.service
install -Dm644 shared-control-node.service %{buildroot}%{_userunitdir}/shared-control-node.service

%files master
%{_bindir}/shared-control-master
%{_udevrulesdir}/60-shared-control.rules
%{_udevrulesdir}/98-shared-control-master.rules
%{_userunitdir}/shared-control-master.service

%files node
%{_bindir}/shared-control-node
%{_udevrulesdir}/60-shared-control.rules
%{_udevrulesdir}/98-shared-control-node.rules
%{_userunitdir}/shared-control-node.service

%post master
udevadm control --reload-rules >/dev/null 2>&1 || :
udevadm trigger --subsystem-match=hidraw >/dev/null 2>&1 || :

%post node
udevadm control --reload-rules >/dev/null 2>&1 || :
udevadm trigger --subsystem-match=hidraw >/dev/null 2>&1 || :

%changelog
* Fri Nov 20 2026 Patrik <patrik@localhost> - 0.1.0-6
- Send each injected event to a single input device and log the offered devices

* Fri Nov 20 2026 Patrik <patrik@localhost> - 0.1.0-5
- Log slow HID writes, event loop stalls and dongle report counts on peer timeout

* Fri Nov 20 2026 Patrik <patrik@localhost> - 0.1.0-4
- Hold injected mouse buttons for at least 30 ms and log why the peer timed out

* Fri Nov 20 2026 Patrik <patrik@localhost> - 0.1.0-3
- Exit cleanly when the dongle is removed and do not restart the service

* Fri Nov 20 2026 Patrik <patrik@localhost> - 0.1.0-2
- Rename the uaccess rule to 60-shared-control.rules so it takes effect

* Fri Nov 20 2026 Patrik <patrik@localhost> - 0.1.0-1
- Initial package
