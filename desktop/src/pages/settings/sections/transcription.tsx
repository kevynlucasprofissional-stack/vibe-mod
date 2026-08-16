import { m } from '~/paraglide/messages.js'
import LanguageInput from '~/components/language-input'
import { InfoTooltip } from '~/components/info-tooltip'
import { Button } from '~/components/ui/button'
import { Switch } from '~/components/ui/switch'
import { SectionCard, type SettingsViewModel } from './shared'

export function TranscriptionSection({ vm }: { vm: SettingsViewModel }) {
	const isPortuguese = vm.preference.displayLanguage === 'pt-BR'
	const chunkingEnabled = vm.preference.modelOptions.chunking_enabled !== false
	const engine = vm.preference.modelMetadata?.capabilities.engine
	const engineUsesNativeChunking = engine === 'nemotron' || engine === 'parakeet'
	const setChunkingEnabled = (checked: boolean) => {
		vm.preference.setModelOptions({ ...vm.preference.modelOptions, chunking_enabled: checked })
	}

	return (
		<div className="space-y-5">
			<SectionCard>
				<LanguageInput />
			</SectionCard>
			<SectionCard>
				<div className="flex flex-wrap items-center justify-between gap-3 py-1">
					<div className="min-w-0 flex-1">
						<p className="text-sm font-medium">{isPortuguese ? 'Proteção para arquivos longos (Whisper)' : 'Long-file protection (Whisper)'}</p>
						<p className="mt-1 text-xs leading-relaxed text-muted-foreground">
							{isPortuguese
								? 'Para modelos Whisper, envia arquivos longos em requisições independentes de até 30 segundos, com 2 segundos de sobreposição entre janelas, e recompõe os timestamps sem apagar variações ambíguas na fronteira.'
								: 'For Whisper models, sends long files as independent requests of up to 30 seconds with 2 seconds of overlap between windows, then rebuilds timestamps without deleting ambiguous boundary variants.'}
						</p>
					</div>
					<Switch checked={chunkingEnabled} onCheckedChange={setChunkingEnabled} disabled={engineUsesNativeChunking} />
				</div>
				{engineUsesNativeChunking && (
					<p className="mt-3 border-t border-border/45 pt-3 text-xs text-muted-foreground">
						{isPortuguese
							? `O engine ${engine} já faz chunking nativo no Sona; esta proteção externa não é aplicada.`
							: `The ${engine} engine already uses Sona-native chunking, so this external protection is not applied.`}
					</p>
				)}
				{chunkingEnabled && vm.preference.diarizeEnabled && !engineUsesNativeChunking && (
					<p className="mt-3 border-t border-border/45 pt-3 text-xs text-muted-foreground">
						{isPortuguese
							? 'Com diarização, a proteção externa do Whisper é ignorada porque o Sona v0.3.5 não fornece identidade global de locutor entre requisições.'
							: 'With diarization, external Whisper protection is bypassed because Sona v0.3.5 does not provide global speaker identity across requests.'}
					</p>
				)}
			</SectionCard>
			<SectionCard>
				<div className="flex flex-wrap items-center justify-between gap-2 border-b border-border/45 py-2">
					<span className="text-sm font-medium">{m.playSoundOnFinish()}</span>
					<Switch checked={vm.preference.soundOnFinish} onCheckedChange={vm.preference.setSoundOnFinish} />
				</div>
				<div className="flex flex-wrap items-center justify-between gap-2 pb-1 pt-4">
					<span className="text-sm font-medium">{m.focusWindowOnFinish()}</span>
					<Switch checked={vm.preference.focusOnFinish} onCheckedChange={vm.preference.setFocusOnFinish} />
				</div>
			</SectionCard>
			<div className="space-y-2">
				<div className="flex items-center gap-1 px-1">
					<InfoTooltip text={m.recordingSavePathInfo()} />
					<span className="text-sm font-semibold text-foreground/95">{m.recordingSavePath()}</span>
				</div>
				<SectionCard>
					<div className="flex items-center justify-between gap-2">
						<p className="min-w-0 truncate text-sm text-muted-foreground" title={vm.preference.customRecordingPath ?? vm.defaultRecordingPath}>
							{vm.preference.customRecordingPath ?? vm.defaultRecordingPath}
						</p>
						<div className="flex shrink-0 items-center gap-2">
							{vm.preference.customRecordingPath && (
								<Button variant="ghost" size="sm" onMouseDown={vm.resetRecordingPath}>
									{m.resetToDefault()}
								</Button>
							)}
							<Button variant="outline" size="sm" onMouseDown={vm.changeRecordingPath}>
								{m.changeRecordingPath()}
							</Button>
						</div>
					</div>
				</SectionCard>
			</div>
		</div>
	)
}
