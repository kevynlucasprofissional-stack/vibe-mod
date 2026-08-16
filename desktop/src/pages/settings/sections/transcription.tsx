import { m } from '~/paraglide/messages.js'
import LanguageInput from '~/components/language-input'
import { InfoTooltip } from '~/components/info-tooltip'
import { Button } from '~/components/ui/button'
import { Switch } from '~/components/ui/switch'
import { SectionCard, type SettingsViewModel } from './shared'

export function TranscriptionSection({ vm }: { vm: SettingsViewModel }) {
	const isPortuguese = vm.preference.displayLanguage === 'pt-BR'
	const chunkingEnabled = vm.preference.modelOptions.chunking_enabled !== false
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
						<p className="text-sm font-medium">{isPortuguese ? 'Proteção para arquivos longos' : 'Long-file protection'}</p>
						<p className="mt-1 text-xs leading-relaxed text-muted-foreground">
							{isPortuguese
								? 'Transcreve em blocos de até 30 segundos, reinicia o contexto entre blocos, detecta repetições e recompõe os timestamps automaticamente.'
								: 'Transcribes in chunks of up to 30 seconds, resets context between chunks, detects repetition, and rebuilds timestamps automatically.'}
						</p>
					</div>
					<Switch checked={chunkingEnabled} onCheckedChange={setChunkingEnabled} />
				</div>
				{chunkingEnabled && vm.preference.diarizeEnabled && (
					<p className="mt-3 border-t border-border/45 pt-3 text-xs text-muted-foreground">
						{isPortuguese
							? 'A proteção é desativada automaticamente durante a diarização para não misturar a identidade dos locutores entre blocos.'
							: 'Protection is automatically bypassed during speaker diarization to avoid mixing speaker identities across chunks.'}
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
