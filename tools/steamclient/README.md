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

Generate and build the bridge using a Proton checkout's Steamworks headers:

```sh
python3 tools/steamclient/gen_steamclient.py --sdk-root /path/to/Proton/lsteamclient
cd tools/steamclient/generated
./build.sh
```

Building requires `clang++`, `ld64.lld`, and `llvm-lipo`, available in the Nix
development shell. No macOS SDK is required.
