import { app } from '@tauri-apps/api'
import { invoke } from '@tauri-apps/api/core'
import { ls } from './fs'
import * as os from '@tauri-apps/plugin-os'

const ISSUE_DIAGNOSTIC_MAX_CHARS = 16000

export async function getPrettyVersion() {
	const appVersion = await app.getVersion()
	const appName = await app.getName()
	let version = `${appName} ${appVersion}`
	const avx2Enabled = await invoke('is_avx2_enabled')
	if (!avx2Enabled) version += ` (older cpu)`
	return version
}

export async function getAppInfo() {
	const appVersion = await getPrettyVersion()
	const commitHash = await invoke('get_commit_hash')
	const avx2 = await invoke<boolean>('is_avx2_enabled')
	const arch = os.arch()
	const platform = os.platform()
	const kVer = os.version()
	const osType = os.type()
	const osVer = os.version()
	const configPath = await invoke<string>('get_models_folder')
	const entries = await ls(configPath)
	const models = entries
		.filter((e) => e.name?.endsWith('.bin') || e.name?.endsWith('.gguf'))
		.map((e) => e.name)
		.join(', ')
	const defaultModel = localStorage.getItem('prefs_model_path')?.split(/[\\/]/)?.pop() ?? 'Not Found'
	const cargoFeatures = (await invoke<string[]>('get_cargo_features')) || []
	return [
		`App Version: ${appVersion}`,
		`Commit Hash: ${commitHash}`,
		`Arch: ${arch}`,
		`Platform: ${platform}`,
		`Kernel Version: ${kVer}`,
		`OS: ${osType}`,
		`OS Version: ${osVer}`,
		`Models: ${models}`,
		`Default Model: ${defaultModel}`,
		`Cargo features: ${cargoFeatures.join(', ')}`,
		`AVX2: ${avx2}`,
	].join('\n')
}

export async function collectLogs() {
	try {
		let info = await getAppInfo()
		const diagnostic = await invoke<string | null>('get_latest_diagnostic_report_content')
		if (diagnostic) {
			const excerpt = compactDiagnosticForIssue(diagnostic)
			info += `\n\n<details>\n<summary>latest structured diagnostic report (excerpt)</summary>\n\n\`\`\`json\n${excerpt}\n\`\`\`\n\nFull report: use Settings > Advanced > Copy latest diagnostic / Show latest diagnostic and attach the JSON file when needed.\n</details>\n`
			return info
		}

		const logs = await invoke<string>('get_logs')
		const relevantLogs = logs
			.split('\n')
			.filter((line) => {
				const lower = line.toLowerCase()
				return lower.includes('error') || lower.includes('warn')
			})
			.slice(-50)
			.join('\n')
		info += `\n\n<details>\n<summary>raw warning/error log fallback</summary>\n\n\`\`\`console\n${relevantLogs}\n\`\`\`\n</details>\n`
		return info
	} catch (error) {
		console.error(error)
		return `Couldn't collect diagnostic information: ${error}`
	}
}

function compactDiagnosticForIssue(diagnostic: string) {
	if (diagnostic.length <= ISSUE_DIAGNOSTIC_MAX_CHARS) return diagnostic
	const headChars = 11000
	const tailChars = ISSUE_DIAGNOSTIC_MAX_CHARS - headChars
	return `${diagnostic.slice(0, headChars)}\n\n... <diagnostic truncated for issue URL; attach full JSON> ...\n\n${diagnostic.slice(-tailChars)}`
}
