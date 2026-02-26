# Build Native Module with GitHub Actions

This project uses GitHub Actions to build the native module without requiring local Xcode or other platform-specific toolchains.

## Building Locally

If you want to build locally (requires Rust, Node.js, and Xcode on macOS):

```bash
npm install
napi build --platform --release
```

## Building with GitHub Actions

### Option 1: Create a Release Tag (Recommended)

1. Commit and push your changes:

```bash
git add .
git commit -m "Your commit message"
git push origin main
```

2. Create and push a tag:

```bash
git tag v0.1.0
git push origin v0.1.0
```

3. A GitHub Release will be automatically created with zip files for all platforms.

### Option 2: Manual Trigger

1. Go to your repository on GitHub
2. Click **Actions** tab
3. Select **Build Native Module** workflow
4. Click **Run workflow** → **Run workflow**

4. After the build completes, download the artifacts from the workflow run page.

## Downloads

After a successful build, you'll get:

| Artifact | Description |
|----------|-------------|
| `natively-audio-darwin-x64.zip` | macOS Intel (x64) |
| `natively-audio-darwin-arm64.zip` | macOS Apple Silicon |
| `natively-audio-linux-x64-gnu.zip` | Linux x64 |
| `natively-audio-linux-arm64-gnu.zip` | Linux ARM64 |
| `natively-audio-win32-x64-msvc.zip` | Windows x64 |
| `natively-audio-builds.zip` | All platforms combined (manual trigger only) |

## Using the Builds

### For macOS:

```bash
# Download the appropriate zip
unzip natively-audio-darwin-arm64.zip  # or -x64 for Intel

# The .node file is ready to use
```

### For Windows:

```powershell
# Download natively-audio-win32-x64-msvc.zip
Expand-Archive natively-audio-win32-x64-msvc.zip -DestinationPath .
```

### For Linux:

```bash
unzip natively-audio-linux-x64-gnu.zip
```

## Setting Up Your Repository

1. Create a new repository on GitHub (if you haven't already)
2. Initialize git and push:

```bash
git init
git add .
git commit -m "Initial commit"
git branch -M main
git remote add origin https://github.com/YOUR_USERNAME/natively-rust-build.git
git push -u origin main
```

3. Go to **Settings** → **Actions** → **General** → **Workflow permissions**
4. Enable **"Read and write permissions"**

## Built Platforms

The workflow builds for the following platforms:

| Platform | Architecture | Runner OS |
|----------|--------------|-----------|
| macOS | x64 (Intel) | macos-12 |
| macOS | arm64 (Apple Silicon) | macos-14 |
| Linux | x64 (GNU) | ubuntu-20.04 |
| Linux | arm64 (GNU) | ubuntu-20.04-arm64 |
| Windows | x64 (MSVC) | windows-2019 |
