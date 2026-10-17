{
  lib,
  stdenv,
  runtimeShell,
  rustPlatform,
  pkg-config,
  makeWrapper,
  bash,
  pipewire,
  wayland,
  wayland-scanner,
  pulseaudio,
  curl,
  glib,
  wineWow64Packages,
  ethtool,
  networkmanager,
  pkgsCross,
}:

let
  windowsCC = pkgsCross.mingwW64.stdenv.cc;
  windowsBinutils = pkgsCross.mingwW64.buildPackages.binutils;
  targetPrefix = windowsCC.targetPrefix;
in
rustPlatform.buildRustPackage {
  pname = "uur";
  version = "0.1.6";

  src = lib.cleanSource ../.;
  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [
    pkg-config
    makeWrapper
    windowsCC
    windowsBinutils
    bash
    wayland-scanner
  ];
  buildInputs = [ pipewire wayland ];

  CC = "${stdenv.cc}/bin/cc";
  UUR_BUILD_SHELL = runtimeShell;
  doCheck = false;

  postPatch = ''
    patchShebangs hook/build.sh capture/build.sh packaging/stage.sh
  '';

  postBuild = ''
    UU_MINGW_CC=${windowsCC}/bin/${targetPrefix}cc \
    UU_MINGW_STRIP=${windowsBinutils}/bin/${targetPrefix}strip \
    UU_MINGW_DLLTOOL=${windowsBinutils}/bin/${targetPrefix}dlltool \
      ./hook/build.sh
    ./capture/build.sh
  '';

  installPhase = ''
    runHook preInstall
    UUR_BINARY=target/${stdenv.hostPlatform.rust.rustcTarget}/release/uur \
      ./packaging/stage.sh "$out" ""
    wrapProgram "$out/bin/uur" \
      --prefix PATH : ${lib.makeBinPath [
        curl
        bash
        glib
        pulseaudio
        wineWow64Packages.stable
        ethtool
        networkmanager
      ]}
    runHook postInstall
  '';

  meta = {
    description = "Native Linux companion for NetEase UU Remote";
    homepage = "https://github.com/panxuc/uur";
    license = lib.licenses.mit;
    mainProgram = "uur";
    platforms = [ "x86_64-linux" ];
  };
}
