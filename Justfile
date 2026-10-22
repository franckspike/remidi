# Justfile for ReMIDI

# Par défaut, afficher la liste des recettes disponibles
default:
    @just --list

# Compiler le projet en mode debug
build:
    cargo build

# Compiler le projet en mode release optimisé
release:
    cargo build --release

# Générer l'installateur Windows (remidi-setup.exe) avec Inno Setup
installer: release
    @if [ -f "/c/Program Files (x86)/Inno Setup 6/ISCC.exe" ]; then \
        "/c/Program Files (x86)/Inno Setup 6/ISCC.exe" installer/remidi.iss; \
    elif command -v iscc >/dev/null 2>&1; then \
        iscc installer/remidi.iss; \
    else \
        echo "Erreur: Inno Setup (iscc) n'a pas été trouvé dans le PATH ni dans Program Files."; \
        exit 1; \
    fi
    @echo "Installateur généré dans target/installer/remidi-setup.exe !"


# Lancer ReMIDI en mode debug
run *args:
    cargo run -- {{args}}

# Lancer la version release
run-release *args:
    cargo run --release -- {{args}}

# Exécuter les tests unitaires et d'intégration
test:
    cargo test

# Vérifier la compilation sans générer de binaire
check:
    cargo check

# Linter avec clippy
clippy:
    cargo clippy --all-targets

# Formater le code source
fmt:
    cargo fmt

# Vérifier le formatage sans modifier les fichiers
fmt-check:
    cargo fmt --check

# Installer le binaire localement via cargo et enregistrer le raccourci XDG (Linux)
install:
    cargo install --path .
    @if [ "$(uname)" = "Linux" ]; then \
        echo "Installation du raccourci XDG et de l'icône..."; \
        mkdir -p "${XDG_DATA_HOME:-$HOME/.local/share}/applications"; \
        mkdir -p "${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/scalable/apps"; \
        BIN_PATH="$(which remidi 2>/dev/null || echo "$HOME/.cargo/bin/remidi")"; \
        sed "s|^Exec=remidi|Exec=$BIN_PATH|" assets/remidi.desktop > "${XDG_DATA_HOME:-$HOME/.local/share}/applications/remidi.desktop"; \
        chmod +x "${XDG_DATA_HOME:-$HOME/.local/share}/applications/remidi.desktop"; \
        cp assets/remidi.svg "${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/scalable/apps/remidi.svg"; \
        if command -v update-desktop-database >/dev/null 2>&1; then \
            update-desktop-database "${XDG_DATA_HOME:-$HOME/.local/share}/applications" >/dev/null 2>&1 || true; \
        fi; \
        if command -v gtk-update-icon-cache >/dev/null 2>&1; then \
            gtk-update-icon-cache -f -t "${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor" >/dev/null 2>&1 || true; \
        fi; \
        echo "ReMIDI installé dans le menu d'applications XDG !"; \
    fi

# Désinstaller le binaire et le raccourci XDG
uninstall:
    cargo uninstall remidi
    @if [ "$(uname)" = "Linux" ]; then \
        rm -f "${XDG_DATA_HOME:-$HOME/.local/share}/applications/remidi.desktop"; \
        rm -f "${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/scalable/apps/remidi.svg"; \
        if command -v update-desktop-database >/dev/null 2>&1; then \
            update-desktop-database "${XDG_DATA_HOME:-$HOME/.local/share}/applications" >/dev/null 2>&1 || true; \
        fi; \
        echo "Raccourci XDG retiré."; \
    fi

# Afficher l'état des ports et clients ALSA MIDI (Linux)
alsa-ports:
    aconnect -l

# Ouvrir le fichier de configuration utilisateur dans $EDITOR
edit-config:
    ${EDITOR:-nano} ~/.config/remidi/config.toml

# Nettoyer les fichiers de build
clean:
    cargo clean
