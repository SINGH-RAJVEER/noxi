{ pkgs, rust-overlay, desktop ? false }:
let
	inherit (pkgs) lib;
	rustPkgs = pkgs.extend (import rust-overlay);
	toolchain = rustPkgs.rust-bin.selectLatestNightlyWith (t: t.minimal);
	rustPlatform = pkgs.makeRustPlatform {
		cargo = toolchain;
		rustc = toolchain;
	};
	workspace = (builtins.fromTOML (builtins.readFile ../../settings/Cargo.toml)).workspace;
	targets = if desktop then [ "noxios-settings-gui" ] else [ "noxios-settings-cli" "noxios-settings-tui" ];
	flags = lib.concatMap (target: [ "-p" target ]) targets ++ lib.optionals desktop [ "--features" "native" ];
	guiLibraries = with pkgs; [
		vulkan-loader
		wayland
		libxkbcommon
		libx11
		libxcb
	];
in
rustPlatform.buildRustPackage {
	pname = if desktop then "noxios-settings-desktop" else "noxios-settings-console";
	inherit (workspace.package) version;
	src = lib.fileset.toSource {
		root = ../../settings;
		fileset = lib.fileset.unions [
			../../settings/assets
			../../settings/crates
			../../settings/Cargo.toml
			../../settings/Cargo.lock
			../../settings/LICENSE
		];
	};
	cargoLock.lockFile = ../../settings/Cargo.lock;
	cargoBuildFlags = flags;
	cargoInstallFlags = flags;
	nativeBuildInputs = lib.optionals desktop (with pkgs; [ makeWrapper pkg-config desktop-file-utils ]);
	buildInputs = lib.optionals desktop (guiLibraries ++ [ pkgs.fontconfig pkgs.freetype ]);
	# The settings workspace tests run in the development shell. Some fixtures
	# need a Nix daemon, a writable worktree, or a graphical test context.
	doCheck = false;
	postInstall = ''
		install -Dm644 LICENSE "$out/share/licenses/noxios-settings/LICENSE"
	'' + lib.optionalString desktop ''
		wrapProgram "$out/bin/noxios-settings-gui" \
			--prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath guiLibraries}
		install -Dm644 assets/noxios-settings.svg "$out/share/icons/hicolor/scalable/apps/noxios-settings.svg"
		install -Dm644 assets/noxios-settings.desktop.in "$out/share/applications/noxios-settings.desktop"
		substituteInPlace "$out/share/applications/noxios-settings.desktop" \
			--replace-fail '@NOXIOS_SETTINGS_EXEC@' 'noxios-settings-gui'
		desktop-file-validate "$out/share/applications/noxios-settings.desktop"
	'';
	meta = {
		description = "Built-in settings for noxios";
		license = lib.licenses.asl20;
		platforms = lib.platforms.linux;
	};
}
