//Credit: Much of this was stolen from zigtools
// The module 'vscode' contains the VS Code extensibility API
// Import the module and reference it with the alias vscode in your code below
import * as vscode from 'vscode';
import { workspace, window, ExtensionContext } from 'vscode';
import * as path from "path";
import * as os from "os";
import * as which from "which";
import * as fs from "fs";
import * as mkdirp from "mkdirp";
import * as admzip from "adm-zip";
import * as child_process from "child_process";
import {
	LanguageClient,
	LanguageClientOptions,
	ServerOptions,
	Trace,
	TransportKind
} from 'vscode-languageclient/node';
import axios from "axios";
import { manual } from 'mkdirp';
import { posix } from 'path';
import AdmZip = require('adm-zip');
import { start } from 'repl';

let client: LanguageClient|null=null;
let outputChannel;
const SOURCE_URI = "https://api.github.com/repos/caradhrass/uvls/releases/latest"



function getDefaultInstallationName(): string | null {
	// NOTE: Not using a JS switch because they're ugly as hell and clunky :(

	const plat = process.platform;
	const arch = process.arch;
	if (arch === "x64") {
		if (plat === "linux") return "x86_64-linux";
		else if (plat === "darwin") return "x86_64-macos";
		else if (plat === "win32") return "x86_64-windows";
	} else if (arch === "arm64") {
		if (plat === "darwin") return "aarch64-macos";
		if (plat === "linux") return "aarch64-linux";
	}

	return null;
}
interface Asset {
	name: string,
	browser_download_url: string

}
interface Metadata {
	tag_name: string,
	assets: [Asset],
}
async function fetchInfo(): Promise<Metadata> {
	return (await axios.get<Metadata>(SOURCE_URI)).data
}


async function uvlsPath(context: ExtensionContext) {
	const configuration = workspace.getConfiguration("uvls");
	var uvlsPath = configuration.get<string | null>("path", null);

	if (!uvlsPath) {
		uvlsPath = which.sync('uvls', { nothrow: true });
	} else if (uvlsPath.startsWith("~")) {
		uvlsPath = path.join(os.homedir(), uvlsPath.substring(1));
	} else if (!path.isAbsolute(uvlsPath)) {
		uvlsPath = which.sync(uvlsPath, { nothrow: true });
	}
	const uvlsPathExists = uvlsPath !== null && fs.existsSync(uvlsPath);
	var message: string | null = null;
	if (uvlsPath && uvlsPathExists) {
		try {
			fs.accessSync(uvlsPath, fs.constants.R_OK | fs.constants.X_OK);
		} catch {
			message = `\`uvls.path\` ${uvlsPath} is not an executable`;
		}
		const stat = fs.statSync(uvlsPath);
		if (!stat.isFile()) {
			message = `\`uvls.path\` ${uvlsPath} is not a file`;
		}
	}
	if (message === null) {
		if (!uvlsPath) {
			message = "Couldn't find UVL Language Server (UVLS) executable, please specify it under \`uvls.path\`";
		} else if (!uvlsPathExists) {
			message = `Couldn't find UVL Language Server (UVLS) executable at ${uvlsPath}`;
		}
	}
	if (message) {
		const response = await window.showWarningMessage(message, "Install UVLS", "Specify Path");
		if (response === "Install UVLS") {
			return await installExecutable(context);
		} else if (response === "Specify Path") {
			const uris = await window.showOpenDialog({
				canSelectFiles: true,
				canSelectFolders: false,
				canSelectMany: false,
				title: "Select UVLS executable",
			});

			if (uris) {
				await configuration.update("path", uris[0].path, true);
				return uris[0].path;
			}
		}
		return null;
	}

	return uvlsPath

}
async function installExecutable(context: ExtensionContext): Promise<string | null> {
	const def = getDefaultInstallationName();
	if (!def) {
		window.showInformationMessage(`Your system isn't built by our CI!\nPlease follow the instructions [here](https://github.com/Caradhrass/uvls) to get started!`);
		return null;
	}
	let archiveName = def.concat(".zip");




	return window.withProgress({
		title: "Installing UVLS...",
		location: vscode.ProgressLocation.Notification,
	}, async progress => {
		progress.report({ message: "Downloading UVLS executable..." });
		let meta = await fetchInfo();
		let tgt = meta.assets.find(e => e.name.endsWith(archiveName));
		if (tgt === undefined) {
			window.showInformationMessage(`Your system isn't built by our CI!\nPlease follow the instructions [here](https://github.com/Caradhrass/uvls) to get started!`);
			return null;
		}
		const url = tgt?.browser_download_url;
		const data = (await axios.get(url!, { responseType: "arraybuffer" })).data;
		const zip = new AdmZip(data);
		const folder = `uvls-${meta.tag_name}-${def}`;
		const name = `uvls${def.endsWith("windows") ? ".exe" : ""}`;

		progress.report({ message: "Installing..." });
		zip.extractEntryTo(`${folder}/${name}`, context.globalStorageUri.fsPath, false,true);
		const installDir = context.globalStorageUri;
		const uvlsBinPath = vscode.Uri.joinPath(installDir, name).fsPath;
		fs.chmodSync(uvlsBinPath, 0o755);

		let config = workspace.getConfiguration("uvls");
		await config.update("path", uvlsBinPath, true);

		return uvlsBinPath;
	});
}
interface Version {
	major: number,
	minor: number,
	patch: number,
}

function parseVersion(str: string): Version | null {
	const matches = /v(\d+)\.(\d+)\.(\d+)/.exec(str);
	//                  0   . 10   .  0  -dev .218   +d0732db
	//                                  (         optional          )?

	if (!matches) return null;
	if (matches.length !== 4 && matches.length !== 7) return null;
	return {
		major: parseInt(matches[1]),
		minor: parseInt(matches[2]),
		patch: parseInt(matches[3]),
	};
}
async function isUpdateAvailable(uvlsPath: string): Promise<boolean | null> {
	let remote = parseVersion(await (await fetchInfo()).tag_name);
	const current = parseVersion(child_process.execFileSync(uvlsPath, ['-v']).toString("utf-8"));
	if (!current || !remote) return null;
	if (remote.major < current.major) return false;
	if (remote.major > current.major) return true;
	if (remote.minor < current.minor) return false;
	if (remote.minor > current.minor) return true;
	if (remote.patch < current.patch) return false;
	if (remote.patch > current.patch) return true;
	return false;
}
async function isUVLSPrebuildBinary(context: ExtensionContext): Promise<boolean> {
	const configuration = workspace.getConfiguration("uvls");
	var uvlsPath = configuration.get<string | null>("path", null);
	if (!uvlsPath) return false;
	const uvlsBinPath = vscode.Uri.joinPath(context.globalStorageUri, "uvls").fsPath;
	return uvlsPath.startsWith(uvlsBinPath);
}

async function checkUpdate(context: ExtensionContext, autoInstallPrebuild: boolean): Promise<void> {
	const configuration = workspace.getConfiguration("uvls");

	const p = await uvlsPath(context);
	if (!p) return;

	if (!await isUpdateAvailable(p)) return;

	const isPrebuild = await isUVLSPrebuildBinary(context);

	if (autoInstallPrebuild && isPrebuild) {
		await installExecutable(context);
	} else {
		const message = `There is a new update available for UVLS. ${!isPrebuild ? "It would replace your installation with a prebuilt binary." : ""}`;
		const response = await window.showInformationMessage(message, "Install update", "Never ask again");

		if (response === "Install update") {
			await installExecutable(context);
		} else if (response === "Never ask again") {
			await configuration.update("auto_update", false, true);
		}
	}
}
async function checkUpdateMaybe(context: ExtensionContext) {
	const configuration = workspace.getConfiguration("uvls");
	const checkForUpdate = configuration.get<boolean>("auto_update", true);
	if (checkForUpdate) await checkUpdate(context, true);
}

export async function activate(context: vscode.ExtensionContext) {

	vscode.commands.registerCommand('uvls.check_for_updates', async () => {
		await stopClient();
		await checkUpdate(context,false);
		await startClient(context);
	});
	vscode.commands.registerCommand('uvls.restart', async () => {
		await stopClient();
		await startClient(context);
	});
	await checkUpdateMaybe(context);
	await startClient(context);

}


// This method is called when your extension is deactivated
export function deactivate(): Thenable<void> | undefined {
	if (!client) {
		return undefined;
	}
	return client.stop();
}
async function startClient(context: ExtensionContext) {
	const path = await uvlsPath(context);
	if (!path) {
		window.showWarningMessage("Couldn't find Zig Language Server (UVLS) executable");
		return;
	}
	outputChannel = vscode.window.createOutputChannel("UVL Language Server");
	const serverOptions: ServerOptions = {
		command: path, // Replace with your own command.,
	};

	const clientOptions: LanguageClientOptions = {
		documentSelector: [{ scheme: "file", language: "uvl" },{scheme:"file",pattern:"**/*.uvl.json"}],
		outputChannel,
		
	
	};
	outputChannel.appendLine("test")
	client = new LanguageClient('uvls', serverOptions, clientOptions);
	client.setTrace(Trace.Verbose);
	client.start();
}
async function stopClient(): Promise<void> {
	if (client) client.stop();
	client = null;
  }