#!/usr/bin/env ruby
# frozen_string_literal: true

require "sqlite3"

DB_PATH = File.expand_path("~/.local/share/dejiny/history.db")
HOSTNAME = `hostname`.strip
COUNT = 50_000

# Realistic command fragments to combine
GIT_COMMANDS = [
  "git status",
  "git diff",
  "git diff --staged",
  "git add -p",
  "git add .",
  "git commit -m 'fix typo'",
  "git commit -m 'update deps'",
  "git commit -m 'refactor auth module'",
  "git commit -m 'add pagination'",
  "git commit -m 'fix failing tests'",
  "git commit --amend --no-edit",
  "git push",
  "git push origin main",
  "git push -u origin feature/auth",
  "git pull",
  "git pull --rebase",
  "git checkout main",
  "git checkout -b feature/new-api",
  "git checkout -b fix/login-redirect",
  "git switch -c feature/dashboard",
  "git log --oneline -20",
  "git log --oneline --graph",
  "git log --author='zach'",
  "git stash",
  "git stash pop",
  "git rebase main",
  "git rebase -i HEAD~3",
  "git cherry-pick abc1234",
  "git branch -d old-feature",
  "git branch -D experiment",
  "git merge feature/auth",
  "git fetch origin",
  "git remote -v",
  "git blame src/main.rs",
  "git bisect start",
  "git reset HEAD~1",
  "git clean -fd",
  "git tag v1.2.0",
]

CARGO_COMMANDS = [
  "cargo build",
  "cargo build --release",
  "cargo run",
  "cargo run -- serve --port 8080",
  "cargo test",
  "cargo test -- --nocapture",
  "cargo test auth_tests",
  "cargo test --release",
  "cargo check",
  "cargo clippy",
  "cargo clippy -- -W clippy::pedantic",
  "cargo fmt",
  "cargo fmt --check",
  "cargo doc --open",
  "cargo add serde",
  "cargo add tokio --features full",
  "cargo add clap --features derive",
  "cargo remove unused-crate",
  "cargo update",
  "cargo bench",
  "cargo install ripgrep",
  "cargo install --path .",
  "cargo publish --dry-run",
]

DOCKER_COMMANDS = [
  "docker ps",
  "docker ps -a",
  "docker images",
  "docker build -t myapp .",
  "docker build -t myapp:latest -f Dockerfile.prod .",
  "docker run -it --rm myapp",
  "docker run -d -p 8080:8080 myapp",
  "docker compose up",
  "docker compose up -d",
  "docker compose down",
  "docker compose logs -f",
  "docker compose logs -f api",
  "docker exec -it postgres_db psql -U postgres",
  "docker stop $(docker ps -q)",
  "docker system prune -f",
  "docker pull postgres:16",
  "docker volume ls",
  "docker network ls",
]

SHELL_COMMANDS = [
  "ls",
  "ls -la",
  "ls -lah",
  "ls src/",
  "ll",
  "pwd",
  "cd ~/projects/dejiny",
  "cd ~/projects/webapp",
  "cd ~/projects/api-server",
  "cd ..",
  "cd -",
  "cat README.md",
  "cat Cargo.toml",
  "cat .env",
  "head -50 src/main.rs",
  "tail -f /var/log/system.log",
  "tail -100 logs/app.log",
  "mkdir -p src/handlers",
  "rm -rf target/",
  "rm tmp/*.log",
  "cp .env.example .env",
  "mv old_name.rs new_name.rs",
  "chmod +x scripts/deploy.sh",
  "touch src/lib.rs",
  "wc -l src/**/*.rs",
  "du -sh target/",
  "df -h",
  "which rustc",
  "whoami",
  "date",
  "uptime",
  "echo $PATH",
  "echo $SHELL",
  "env | grep RUST",
  "export RUST_LOG=debug",
  "source .env",
  "history | grep docker",
]

SEARCH_COMMANDS = [
  "grep -r 'TODO' src/",
  "grep -rn 'unwrap()' src/",
  "grep -r 'fn main' .",
  "rg 'error' --type rust",
  "rg 'async fn' src/",
  "rg 'pub struct' --type rust",
  "rg 'impl.*for' src/ -l",
  "find . -name '*.rs' | head -20",
  "find . -name '*.log' -delete",
  "fd -e rs",
  "fd -e toml",
  "ag 'deprecated' src/",
  "fzf",
]

EDITOR_COMMANDS = [
  "vim src/main.rs",
  "vim Cargo.toml",
  "vim .env",
  "nvim src/main.rs",
  "nvim src/lib.rs",
  "nvim src/handlers/auth.rs",
  "nvim config/settings.toml",
  "code .",
  "code src/main.rs",
]

NETWORK_COMMANDS = [
  "curl http://localhost:8080/health",
  "curl -s http://localhost:3000/api/users | jq .",
  "curl -X POST http://localhost:8080/api/login -d '{\"user\":\"admin\"}'",
  "curl -I https://example.com",
  "wget https://example.com/file.tar.gz",
  "ssh deploy@prod-server",
  "ssh -i ~/.ssh/prod_key deploy@10.0.1.50",
  "scp build/release/app deploy@prod:/opt/app/",
  "ping google.com",
  "nc -zv localhost 5432",
  "lsof -i :8080",
  "netstat -tlnp",
]

PACKAGE_COMMANDS = [
  "brew update",
  "brew upgrade",
  "brew install jq",
  "brew install ripgrep",
  "brew install fd",
  "brew install postgresql@16",
  "brew services start postgresql@16",
  "brew services stop postgresql@16",
  "npm install",
  "npm run dev",
  "npm run build",
  "npm test",
  "yarn install",
  "pip install -r requirements.txt",
  "pip install requests",
  "rustup update",
  "rustup component add clippy",
]

DB_COMMANDS = [
  "psql -U postgres -d myapp",
  "psql -U postgres -d myapp -c 'SELECT count(*) FROM users'",
  "psql -U postgres -c '\\l'",
  "sqlite3 data.db '.tables'",
  "sqlite3 data.db 'SELECT * FROM migrations'",
  "redis-cli ping",
  "redis-cli KEYS '*session*'",
  "redis-cli FLUSHDB",
]

MISC_COMMANDS = [
  "make",
  "make test",
  "make clean",
  "make deploy",
  "htop",
  "top",
  "ps aux | grep rust",
  "kill -9 12345",
  "killall node",
  "watch -n 2 'cargo test'",
  "time cargo build --release",
  "xargs -I{} echo {}",
  "jq '.data[] | .name' response.json",
  "bat src/main.rs",
  "exa -la",
  "tree src/",
  "tokei",
  "hyperfine 'cargo build'",
  "just build",
  "just test",
  "just deploy",
  "terraform plan",
  "terraform apply",
  "kubectl get pods",
  "kubectl logs -f deploy/api",
  "kubectl apply -f k8s/",
  "gh pr create",
  "gh pr list",
  "gh pr merge 42",
  "gh issue list",
  "gh repo clone org/repo",
  "man grep",
  "tldr tar",
  "python3 scripts/migrate.py",
  "ruby scripts/seed.rb",
  "node scripts/check.js",
  "./scripts/deploy.sh production",
  "./scripts/backup.sh",
  "tar czf backup.tar.gz data/",
  "tar xzf archive.tar.gz",
  "zip -r dist.zip build/",
  "unzip dist.zip",
  "openssl rand -hex 32",
  "base64 <<< 'hello'",
  "sha256sum release.tar.gz",
]

ALL_COMMANDS = [
  *GIT_COMMANDS,
  *CARGO_COMMANDS,
  *DOCKER_COMMANDS,
  *SHELL_COMMANDS,
  *SEARCH_COMMANDS,
  *EDITOR_COMMANDS,
  *NETWORK_COMMANDS,
  *PACKAGE_COMMANDS,
  *DB_COMMANDS,
  *MISC_COMMANDS,
]

CWDS = [
  "/Users/zach/projects/dejiny",
  "/Users/zach/projects/webapp",
  "/Users/zach/projects/api-server",
  "/Users/zach/projects/cli-tool",
  "/Users/zach/projects/infra",
  "/Users/zach/projects/scripts",
  "/Users/zach/dotfiles",
  "/Users/zach",
  "/tmp",
  "/Users/zach/projects/webapp/frontend",
  "/Users/zach/projects/webapp/backend",
  "/Users/zach/projects/data-pipeline",
  "/Users/zach/projects/ml-experiment",
]

# Weight commands so common ones appear more often
WEIGHTED_COMMANDS = [
  *GIT_COMMANDS.flat_map { |c| [c] * 5 },       # git is very frequent
  *CARGO_COMMANDS.flat_map { |c| [c] * 4 },      # cargo too
  *SHELL_COMMANDS.flat_map { |c| [c] * 3 },      # basic shell commands
  *EDITOR_COMMANDS.flat_map { |c| [c] * 3 },     # editing files
  *SEARCH_COMMANDS.flat_map { |c| [c] * 2 },
  *DOCKER_COMMANDS,
  *NETWORK_COMMANDS,
  *PACKAGE_COMMANDS,
  *DB_COMMANDS,
  *MISC_COMMANDS,
]

puts "Seeding #{COUNT} commands into #{DB_PATH}..."

db = SQLite3::Database.new(DB_PATH)
db.execute("PRAGMA journal_mode=WAL")
db.execute("PRAGMA synchronous=NORMAL")

# Start from 6 months ago
base_time = Time.now.to_f - (6 * 30 * 24 * 3600)
current_time = base_time

db.transaction do
  COUNT.times do |i|
    cmd = WEIGHTED_COMMANDS.sample
    cwd = CWDS.sample
    exit_code = rand < 0.85 ? 0 : [1, 2, 126, 127, 128, 130, 137, 255].sample
    duration = rand(0.01..30.0)
    start = current_time
    finish = current_time + duration

    db.execute(
      "INSERT INTO commands (command, exit_code, start, end, cwd, hostname) VALUES (?, ?, ?, ?, ?, ?)",
      [cmd, exit_code, start, finish, cwd, HOSTNAME]
    )

    # Advance time by a random interval (simulating gaps between commands)
    current_time += duration + rand(1.0..600.0) # 1s to 10min between commands

    if (i + 1) % 10_000 == 0
      puts "  #{i + 1}/#{COUNT} inserted..."
    end
  end
end

total = db.get_first_value("SELECT COUNT(*) FROM commands")
distinct = db.get_first_value("SELECT COUNT(DISTINCT command) FROM commands")
puts "Done. #{total} total rows, #{distinct} distinct commands."
