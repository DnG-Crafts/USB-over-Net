# Third-party software

## usbip-win2 (the Windows USB/IP driver)

`usbip-tray.exe` can carry the usbip-win2 installer inside it and installs it on request. usbip-win2
is a separate project by its own authors, released under the BSD 2-Clause licence:
https://github.com/vadimgrn/usbip-win2

If you distribute a build that contains the installer, ship the LICENSE file from that repository
alongside it (BSD-2-Clause requires the copyright notice and licence text to accompany binary
redistributions). The installer and driver are signed by Microsoft; this project does not modify them.
