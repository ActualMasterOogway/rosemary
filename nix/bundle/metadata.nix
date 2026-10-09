# Provenance for every output reachable from `roots` through buildInputs, as an
# attrset keyed by store path, for the bundler's source report.
#
# A superset of what is actually referenced at run time, which is fine: the
# bundler only looks up the store paths it copied from. The keys carry no
# string context, so describing a package does not make it a build input.
{ lib }:
roots:
let
  safe = default: expr: let r = builtins.tryEval expr; in if r.success then r.value else default;

  inputsOf = pkg: lib.filter lib.isDerivation (
    lib.flatten [
      (safe [ ] (pkg.buildInputs or [ ]))
      (safe [ ] (pkg.propagatedBuildInputs or [ ]))
    ]
  );

  closure = builtins.genericClosure {
    startSet = map (pkg: { key = pkg.outPath; inherit pkg; }) roots;
    operator = node: map (pkg: { key = pkg.outPath; inherit pkg; }) (inputsOf node.pkg);
  };

  licenseId = l:
    if builtins.isString l then l
    else l.spdxId or l.shortName or l.fullName or "unknown";

  sourceOf = pkg:
    let
      src = safe null (pkg.src or null);
      pick = name: safe null (src.${name} or null);
    in
    if src == null || !(builtins.isAttrs src) then null
    else lib.filterAttrs (_: v: v != null) {
      urls = pick "urls";
      url = pick "url";
      hash = pick "outputHash";
      rev = pick "rev";
      repo = pick "gitRepoUrl";
    };

  describe = pkg: {
    name = safe "unknown" pkg.name;
    pname = safe null (pkg.pname or null);
    version = safe null (pkg.version or null);
    outputs = safe [ "out" ] (pkg.outputs or [ "out" ]);
    license = safe [ ] (map licenseId (lib.toList (pkg.meta.license or [ ])));
    homepage = safe null (pkg.meta.homepage or null);
    source = safe null (sourceOf pkg);
    patches = safe [ ] (map (p: builtins.unsafeDiscardStringContext (baseNameOf (toString p))) (pkg.patches or [ ]));
  };

  entries = lib.concatMap (node:
    let
      pkg = node.pkg;
      info = describe pkg;
      outputs = safe [ "out" ] (pkg.outputs or [ "out" ]);
    in
    map (o: { name = builtins.unsafeDiscardStringContext (safe node.key pkg.${o}.outPath); value = info // { output = o; }; }) outputs
  ) closure;
in
builtins.listToAttrs entries
