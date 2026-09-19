# AIR fixtures

The textual fixtures are owned synthetic inputs copied from
`validation/fixtures/public` in the sibling `metal2vulkan` checkout. No
implementation code or internal IR from that project is used here.

`kernel_store_const.bc.hex` is the byte-for-byte hexadecimal form of
`kernel_store_const.ll`, assembled once with LLVM 19.1.7:

```text
llvm-as kernel_store_const.ll -o kernel_store_const.bc
xxd -p -c 100000 kernel_store_const.bc
```

The decoded bitcode SHA-256 is
`ffe292496068d195b8c6337ff69bbecab7cdef161eae8ce88a240d4306a1f659`.
Keeping it as hexadecimal lets the source-only fixture remain easy to audit;
tests decode it in memory and require no LLVM installation.
