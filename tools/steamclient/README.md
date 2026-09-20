# SteamClient.dylib bridge

This tool generates a SteamClient.dylib capable of implementing the interface that libsteam_api.dylib
calls into via the same method that Proton uses.

This is simply a library call forwarder: When a macOS application asks for a Steam client service, normally
a real macOS compiled Steam client would service those requests. Instead, we bridge these requests to the
Steam client already running on the host system, which reduces overhead and complexity since we do not have
to run a real macOS Steam client.

Future plans include integrating as a compatibility tool within Steam to allow convenient installation
of macOS games right within the native Steam client.

Most of the code in here is derived from Proton's official `lsteamclient` tool.
