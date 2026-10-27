#!/usr/bin/env python3
"""Builds lamda-<version>.vsix without needing vsce or npm.

    python3 editors/vscode/package.py
    code --install-extension editors/vscode/lamda-0.1.0.vsix
"""
import json
import pathlib
import zipfile

ROOT = pathlib.Path(__file__).resolve().parent
FILES = [
    "package.json",
    "extension.js",
    "language-configuration.json",
    "snippets.json",
    "syntaxes/lamda.tmLanguage.json",
]

CONTENT_TYPES = """<?xml version="1.0" encoding="utf-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension=".json" ContentType="application/json"/>
  <Default Extension=".js" ContentType="application/javascript"/>
  <Default Extension=".vsixmanifest" ContentType="text/xml"/>
</Types>
"""

MANIFEST = """<?xml version="1.0" encoding="utf-8"?>
<PackageManifest Version="2.0.0" xmlns="http://schemas.microsoft.com/developer/vsx-schema/2011">
  <Metadata>
    <Identity Language="en-US" Id="{name}" Version="{version}" Publisher="{publisher}"/>
    <DisplayName>{displayName}</DisplayName>
    <Description xml:space="preserve">{description}</Description>
    <Categories>Programming Languages</Categories>
    <Properties>
      <Property Id="Microsoft.VisualStudio.Code.Engine" Value="{engine}"/>
    </Properties>
  </Metadata>
  <Installation>
    <InstallationTarget Id="Microsoft.VisualStudio.Code"/>
  </Installation>
  <Dependencies/>
  <Assets>
    <Asset Type="Microsoft.VisualStudio.Code.Manifest" Path="extension/package.json" Addressable="true"/>
  </Assets>
</PackageManifest>
"""


def main():
    pkg = json.loads((ROOT / "package.json").read_text())
    out = ROOT / f"{pkg['name']}-{pkg['version']}.vsix"
    manifest = MANIFEST.format(engine=pkg["engines"]["vscode"], **pkg)
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("[Content_Types].xml", CONTENT_TYPES)
        z.writestr("extension.vsixmanifest", manifest)
        for f in FILES:
            z.write(ROOT / f, f"extension/{f}")
    print(out)


if __name__ == "__main__":
    main()
