# Publishing the Oak VS Code extension

The extension ID is `oak.oak-vcs` (publisher `oak`, marked **preview**).

## Build the package

```bash
cd editors/vscode
npm ci
npm run test:unit
VSCODE_EXECUTABLE="/Applications/Visual Studio Code.app/Contents/MacOS/Code" npm run test:integration
npm run package          # → oak-vcs-<version>.vsix
```

## Visual Studio Marketplace

1. Make sure you own the `oak` publisher at
   <https://marketplace.visualstudio.com/manage>.
2. Either upload the `.vsix` there (**New extension → Visual Studio Code**), or
   publish from the CLI with an Azure DevOps personal access token that has
   the **Marketplace → Manage** scope (organization: *All accessible
   organizations*):

   ```bash
   npx vsce login oak       # paste the PAT once
   npm run publish          # or: npx vsce publish --packagePath oak-vcs-<version>.vsix
   ```

## Open VSX (Cursor, VSCodium, Windsurf, ...)

```bash
npx ovsx create-namespace oak -p <open-vsx-token>   # first time only
npx ovsx publish oak-vcs-<version>.vsix -p <open-vsx-token>
```

## Releasing a new version

Bump `version` in `package.json` (and add a `CHANGELOG.md` entry), rebuild,
and publish again — `npx vsce publish patch|minor` bumps and publishes in one
step. Drop `"preview": true` from `package.json` when it's ready to lose the
Preview badge.
