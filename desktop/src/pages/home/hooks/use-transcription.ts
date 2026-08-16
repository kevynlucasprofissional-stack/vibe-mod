import { event } from '@tauri-apps/api'
import { invoke } from '@tauri-apps/api/core'
import * as webview from '@tauri-apps/api/webviewWindow'
import * as dialog from '@tauri-apps/plugin-dialog'
import { useContext, useEffect, useRef, useState } from 'react'
import { m } from '~/paraglide/messages.js'
import { toast } from 'sonner'
import successSound from '~/assets/success.mp3'
import { analyticsEvents, trackAnalyticsEvent } from '~/lib/analytics'
import * as config from '~/lib/config'
import { finishDiagnosticRun, normalizeError, recordDiagnosticEvent, startDiagnosticRun } from '~/lib/diagnostics'
import { startKeepAwake, stopKeepAwake } from '~/lib/keep-awake'
import { isUserError } from '~/lib/sona-errors'
import * as transcript from '~/lib/transcript'
import { ErrorModalContext } from '~/providers/error-modal'
import { usePreferenceProvider } from '~/providers/preference'

interface UseTranscriptionOptions {
	onResetSummary: () => void
	onSummarize: (segments: transcript.Segment[], prompt: string, diagnosticRunId?: string) => Promise<boolean>
}

export function useTranscription({ onResetSummary, onSummarize }: UseTranscriptionOptions) {
	const preference = usePreferenceProvider()
	const preferenceRef = useRef(preference)
	const { setState: setErrorModal } = useContext(ErrorModalContext)
	const abortRef = useRef(false)
	const [loading, setLoading] = useState(false)
	const [isAborting, setIsAborting] = useState(false)
	const [segments, setSegments] = useState<transcript.Segment[] | null>(null)
	const [progress, setProgress] = useState<number | null>(0)

	useEffect(() => {
		preferenceRef.current = preference
	}, [preference])

	async function onAbort() {
		setIsAborting(true)
		abortRef.current = true
		event.emit('abort_transcribe')
	}

	async function transcribe(path: string) {
		const current = preferenceRef.current
		let diagnosticRunId: string | null = null
		try {
			diagnosticRunId = await startDiagnosticRun('home_execution', {
				input_path: path,
				model_path: current.modelPath,
				model_engine: current.modelMetadata?.capabilities.engine,
				gpu_device: current.gpuDevice,
				diarization: current.diarizeEnabled,
				stable_timestamps: current.stableTimestampsEnabled,
				chunking_enabled: current.modelOptions.chunking_enabled !== false,
				llm_summary_enabled: current.llmConfig.enabled,
			})
		} catch (error) {
			console.error('failed to initialize Home diagnostics', error)
		}

		const record = async (stage: string, message: string, data: Record<string, unknown> = {}, severity: 'info' | 'warning' | 'error' = 'info') => {
			if (!diagnosticRunId) return
			await recordDiagnosticEvent(diagnosticRunId, stage, message, data, severity, 'home').catch(console.error)
		}

		const avx2 = await invoke<boolean>('is_avx2_enabled')
		if (!avx2) {
			await record('home.preflight_failed', 'AVX2 preflight check failed', {}, 'error')
			if (diagnosticRunId) {
				await finishDiagnosticRun(diagnosticRunId, 'failed', { reason: 'avx2_not_supported' }).catch(console.error)
			}
			trackAnalyticsEvent(analyticsEvents.AVX2_NOT_SUPPORTED)
			await dialog.message(m.avx2NotSupported(), { kind: 'error' })
			return
		}

		startKeepAwake()
		setSegments(null)
		onResetSummary()
		setProgress(0)
		setLoading(true)
		abortRef.current = false
		let completedSegments: transcript.Segment[] = []
		let transcriptionSeconds = 0
		let diagnosticFinalized = false
		trackAnalyticsEvent(analyticsEvents.TRANSCRIBE_STARTED, { source: 'home' })

		try {
			if (!current.modelPath) throw new Error('No model selected. Please download or select a model first.')
			await record('home.model_load_started', 'Loading the selected model before transcription', {
				model_path: current.modelPath,
				gpu_device: current.gpuDevice,
			})
			const loadResult = await invoke<string>('load_model', {
				modelPath: current.modelPath,
				gpuDevice: current.gpuDevice,
				unloadTimeoutMinutes: current.unloadTimeoutMinutes,
			})
			await record('home.model_load_completed', 'Model is ready for Home transcription', { result: loadResult })
			if (loadResult === 'gpu_fallback') toast.warning(m.gpuFallbackToCpu(), { position: 'bottom-center', duration: 8000 })

			const requiresVad = current.modelMetadata?.capabilities.requires_vad ?? false
			const modelsFolder = current.diarizeEnabled || current.stableTimestampsEnabled || requiresVad ? await invoke<string>('get_models_folder') : null
			const diarizeModel = current.diarizeEnabled ? `${modelsFolder}/${config.diarizeModelFilename}` : undefined
			const vadModel = current.stableTimestampsEnabled || requiresVad ? `${modelsFolder}/${config.vadModelFilename}` : undefined
			const options = {
				path,
				...current.modelOptions,
				chunking_enabled: current.modelOptions.chunking_enabled !== false,
				diagnostic_source: 'home',
				...(diagnosticRunId
					? { diagnostic_run_id: diagnosticRunId, diagnostic_item_id: 'transcription' }
					: {}),
				...(diarizeModel ? { diarize_model: diarizeModel } : {}),
				...(vadModel ? { vad_model: vadModel } : {}),
				...(current.stableTimestampsEnabled ? { stable_timestamps: true } : {}),
			}
			const startedAt = performance.now()
			const result = await invoke<transcript.Transcript>('transcribe', { options })
			transcriptionSeconds = Math.round((performance.now() - startedAt) / 1000)
			console.info(`Transcribe took ${transcriptionSeconds} seconds.`)
			completedSegments = result.segments
			setSegments(result.segments)
			toast.success(m.transcribeTook({ total: String(transcriptionSeconds) }), { position: 'bottom-center' })
			trackAnalyticsEvent(analyticsEvents.TRANSCRIBE_SUCCEEDED, {
				source: 'home',
				duration_seconds: transcriptionSeconds,
				segments_count: result.segments.length,
			})
			await record('home.transcription_completed', 'Home transcription completed', {
				duration_seconds: transcriptionSeconds,
				segments: result.segments.length,
			})
		} catch (error) {
			const errorObject = typeof error === 'object' && error !== null ? (error as { code?: string; message?: string }) : null
			const errorMessage = errorObject?.message || String(error)
			const aborted = abortRef.current || errorObject?.code === 'aborted'
			await record(
				aborted ? 'home.transcription_aborted' : 'home.transcription_failed',
				aborted ? 'Home transcription was aborted' : 'Home transcription failed',
				{ error: normalizeError(error) },
				aborted ? 'warning' : 'error',
			)
			if (diagnosticRunId) {
				await finishDiagnosticRun(diagnosticRunId, aborted ? 'aborted' : 'failed', {
					error: normalizeError(error),
				}).catch(console.error)
				diagnosticFinalized = true
			}

			if (!aborted) {
				stopKeepAwake()
				console.error('error: ', error)
				if (errorObject?.code && isUserError(errorObject.code)) {
					toast.error(`${m.error()}: ${errorMessage}`, { position: 'bottom-center' })
				} else {
					trackAnalyticsEvent(analyticsEvents.TRANSCRIBE_FAILED, {
						source: 'home',
						error_message: errorMessage,
						file_ext: path.split('.').pop() ?? 'unknown',
					})
					setErrorModal?.({ log: errorMessage, open: true })
				}
				setLoading(false)
			}
		} finally {
			stopKeepAwake()
			setLoading(false)
			setIsAborting(false)
			setProgress(null)
			if (!abortRef.current) {
				if (preferenceRef.current.soundOnFinish) new Audio(successSound).play()
				if (preferenceRef.current.focusOnFinish) {
					webview.getCurrentWebviewWindow().unminimize()
					webview.getCurrentWebviewWindow().setFocus()
				}
			}
		}

		if (completedSegments.length > 0) {
			let summarySucceeded = true
			if (preferenceRef.current.llmConfig.enabled) {
				summarySucceeded = await onSummarize(completedSegments, preferenceRef.current.llmConfig.prompt, diagnosticRunId ?? undefined)
			}
			if (diagnosticRunId && !diagnosticFinalized) {
				await finishDiagnosticRun(diagnosticRunId, summarySucceeded ? 'succeeded' : 'partial', {
					transcription_seconds: transcriptionSeconds,
					segments: completedSegments.length,
					summary_enabled: preferenceRef.current.llmConfig.enabled,
					summary_succeeded: summarySucceeded,
				}).catch(console.error)
			}
		}
	}

	return { loading, isAborting, segments, setSegments, progress, setProgress, transcribe, onAbort }
}
