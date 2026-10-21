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

# Installer le binaire localement via cargo
install:
    cargo install --path .

# Afficher l'état des ports et clients ALSA MIDI (Linux)
alsa-ports:
    aconnect -l

# Ouvrir le fichier de configuration utilisateur dans $EDITOR
edit-config:
    ${EDITOR:-nano} ~/.config/remidi/config.toml

# Nettoyer les fichiers de build
clean:
    cargo clean
