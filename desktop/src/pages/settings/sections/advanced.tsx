import { m } from '~/paraglide/messages.js'
import { InfoTooltip } from '~/components/info-tooltip'
import { Button } from '~/components/ui/button'
import { Switch } from '~/components/ui/switch'
import { Input } from '~/components/ui/input'
import { ReactComponent as CopyIcon } from '~/icons/copy.svg'
import { ReactComponent as FolderIcon } from '~/icons/folder.svg'
import { ReactComponent as ResetIcon } from '~/icons/reset.svg'
import { SectionCard, type SettingsViewModel } from './shared'
import { invoke } from '@tauri-apps/api/core'
import * as clipboard from '@tauri-apps/plugin-clipboard-manager'
import { toast } from 'sonner'

export function AdvancedSection({ vm }: { vm: SettingsViewModel }) {
	const isPortuguese = vm.preference.displayLanguage === 'pt-BR'

	async function copyLatestDiagnostic() {
		try {
			const content = await invoke<string | null>('get_latest_diagnostic_report_content')
			if (!content) {
				toast.info(isPortuguese ? 'Ainda não existe relatório de diagnóstico.' : 'No diagnostic report exists yet.')
				return
			}
			await clipboard.writeText(content)
			toast.success(isPortuguese ? 'Relatório de diagnóstico copiado.' : 'Diagnostic report copied.')
		} catch (error) {
			toast.error(String(error))
		}
	}

	async function revealLatestDiagnostic() {
		try {
			const found = await invoke<boolean>('show_latest_diagnostic_report')
			if (!found) toast.info(isPortuguese ? 'Ainda não existe relatório de diagnóstico.' : 'No diagnostic report exists yet.')
		} catch (error) {
			toast.error(String(error))
		}
	}

	return (
		<div className="space-y-5">
			<div className="space-y-2">
				<div className="flex items-center gap-1 px-1">
					<InfoTooltip text={m.ytdlpOptionsInfo()} />
					<span className="text-sm font-semibold text-foreground/95">{m.ytdlpOptions()}</span>
				</div>
				<SectionCard>
					<div className="flex flex-wrap items-center justify-between gap-2">
						<span className="text-sm font-medium">{m.checkYtdlpUpdates()}</span>
						<Switch checked={vm.preference.shouldCheckYtDlpVersion} onCheckedChange={vm.preference.setShouldCheckYtDlpVersion} />
					</div>
				</SectionCard>
			</div>
			<div className="space-y-2">
				<div className="flex items-center gap-1 px-1">
					<InfoTooltip text={`${m.unloadModelAfterInactivityInfo()} ${m.zeroMeansNever()}`} />
					<span className="text-sm font-semibold text-foreground/95">{m.modelMemory()}</span>
				</div>
				<SectionCard>
					<div className="flex items-center justify-between gap-4">
						<span className="text-sm font-medium">{m.unloadModelAfterInactivity()}</span>
						<div className="flex items-center gap-2">
							<Input
								type="number"
								min={0}
								max={1440}
								step={1}
								value={vm.preference.unloadTimeoutMinutes}
								onChange={(event) => {
									const minutes = Number(event.target.value)
									if (Number.isFinite(minutes)) vm.preference.setUnloadTimeoutMinutes(Math.min(1440, Math.max(0, Math.floor(minutes))))
								}}
								className="h-6 w-20 rounded-lg px-2 py-0 text-right"
							/>
							<span className="text-sm text-muted-foreground">{m.minutes()}</span>
						</div>
					</div>
				</SectionCard>
			</div>
			<div className="divide-y divide-border/45 rounded-2xl border border-border/60 bg-card/92 shadow-xs">
				<Button
					variant="ghost"
					onMouseDown={copyLatestDiagnostic}
					className="h-12 w-full justify-between rounded-none px-4 font-medium first:rounded-t-2xl last:rounded-b-2xl hover:bg-accent/55">
					{isPortuguese ? 'Copiar último diagnóstico' : 'Copy latest diagnostic'} <CopyIcon className="h-4 w-4 text-muted-foreground" />
				</Button>
				<Button
					variant="ghost"
					onMouseDown={revealLatestDiagnostic}
					className="h-12 w-full justify-between rounded-none px-4 font-medium first:rounded-t-2xl last:rounded-b-2xl hover:bg-accent/55">
					{isPortuguese ? 'Mostrar último diagnóstico' : 'Show latest diagnostic'} <FolderIcon className="h-4 w-4 text-muted-foreground" />
				</Button>
				<Button
					variant="ghost"
					onMouseDown={() => invoke('show_diagnostics_folder')}
					className="h-12 w-full justify-between rounded-none px-4 font-medium first:rounded-t-2xl last:rounded-b-2xl hover:bg-accent/55">
					{isPortuguese ? 'Pasta de diagnósticos' : 'Diagnostics folder'} <FolderIcon className="h-4 w-4 text-muted-foreground" />
				</Button>
				<Button
					variant="ghost"
					onMouseDown={vm.copyLogs}
					className="h-12 w-full justify-between rounded-none px-4 font-medium first:rounded-t-2xl last:rounded-b-2xl hover:bg-accent/55">
					{m.copyLogs()} <CopyIcon className="h-4 w-4 text-muted-foreground" />
				</Button>
				<Button
					variant="ghost"
					onMouseDown={vm.revealLogs}
					className="h-12 w-full justify-between rounded-none px-4 font-medium first:rounded-t-2xl last:rounded-b-2xl hover:bg-accent/55">
					{m.logsFolder()} <FolderIcon className="h-4 w-4 text-muted-foreground" />
				</Button>
				<Button
					variant="ghost"
					onMouseDown={vm.revealTemp}
					className="h-12 w-full justify-between rounded-none px-4 font-medium first:rounded-t-2xl last:rounded-b-2xl hover:bg-accent/55">
					{m.tempFolder()} <FolderIcon className="h-4 w-4 text-muted-foreground" />
				</Button>
				<Button
					variant="ghost"
					onClick={vm.askAndReset}
					className="h-12 w-full justify-between rounded-none px-4 font-medium text-destructive first:rounded-t-2xl last:rounded-b-2xl hover:bg-destructive/12 hover:text-destructive">
					{m.resetApp()} <ResetIcon className="h-5 w-5" />
				</Button>
			</div>
		</div>
	)
}
