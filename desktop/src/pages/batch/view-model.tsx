import { invoke } from '@tauri-apps/api/core'
import { useEffect, useRef, useState } from 'react'
import { m } from '~/paraglide/messages.js'
import { useLocation, useNavigate } from 'react-router-dom'
import { TextFormat, formatExtensions } from '~/components/format-select'
import { Segment, Transcript, asCsv, asJson, asSrt, asText, asVtt } from '~/lib/transcript'
import { isUserError } from '~/lib/sona-errors'
import { NamedPath } from '~/lib/types'
import { pathToNamedPath } from '~/lib/fs'
import { validPath } from '~/lib/media'
import { startKeepAwake, stopKeepAwake } from '~/lib/keep-awake'
import * as webview from '@tauri-apps/api/webviewWindow'
import * as dialog from '@tauri-apps/plugin-dialog'
import * as config from '~/lib/config'
import { analyticsEvents, trackAnalyticsEvent } from '~/lib/analytics'
import successSound from '~/assets/success.mp3'
import * as fs from '@tauri-apps/plugin-fs'
import { emit, listen } from '@tauri-apps/api/event'
import { usePreferenceProvider } from '~/providers/preference'
import { useFilesContext } from '~/providers/files-provider'
import { basename } from '@tauri-apps/api/path'
import { Claude, Ollama, Llm, OpenAICompatible } from '~/lib/llm'
import * as transcript from '~/lib/transcript'
import { path } from '@tauri-apps/api'
import { toDocx } from '~/lib/docx'
import { toast } from 'sonner'
import {
	finishDiagnosticRun,
	normalizeError,
	recordDiagnosticEvent,
	startDiagnosticRun,
	updateDiagnosticItem,
} from '~/lib/diagnostics'

export function viewModel() {
	const { files, setFiles } = useFilesContext()
	const [formats, setFormats] = useState<TextFormat[]>(['normal'])
	const [currentIndex, setCurrentIndex] = useState(0)
	const [progress, setProgress] = useState<number | null>(null)
	const [inProgress, setInProgress] = useState(false)
	const [isAborting, setIsAborting] = useState(false)
	const isAbortingRef = useRef<boolean>(false)
	const preference = usePreferenceProvider()
	const navigate = useNavigate()
	const [llm, setLlm] = useState<Llm | null>(null)
	const location = useLocation()
	const [outputFolder, setOutputFolder] = useState('')

	useEffect(() => {
		if (preference.llmConfig?.platform === 'ollama') setLlm(new Ollama(preference.llmConfig))
		else if (preference.llmConfig?.platform === 'openai') setLlm(new OpenAICompatible(preference.llmConfig))
		else setLlm(new Claude(preference.llmConfig))
	}, [preference.llmConfig])

	const speakerLabel = m.speakerPrefix()
	function getText(segments: Segment[], format: TextFormat) {
		if (format === 'srt') return asSrt(segments, speakerLabel)
		if (format === 'vtt') return asVtt(segments, speakerLabel)
		if (format === 'json') return asJson(segments)
		if (format === 'csv') return asCsv(segments)
		return asText(segments, speakerLabel)
	}

	async function checkFilesState() {
		if (location?.state?.files) {
			const newFiles: NamedPath[] = []
			for (const path of location.state.files) {
				if (!validPath(path)) continue
				const name = await basename(path)
				newFiles.push({ name, path })
			}
			setFiles(newFiles)
		}
	}

	async function checkOutputFolderState() {
		if (location.state?.outputFolder && !outputFolder && (await fs.exists(location.state.outputFolder))) {
			setOutputFolder(location.state.outputFolder)
		}
	}

	useEffect(() => {
		checkOutputFolderState()
	}, [])

	useEffect(() => {
		checkFilesState()
	}, [])

	async function selectFiles() {
		const selected = await dialog.open({
			multiple: true,
			filters: [{ name: 'Audio', extensions: [...config.audioExtensions, ...config.videoExtensions] }],
		})
		if (selected) {
			const newFiles: NamedPath[] = []
			for (const path of selected) {
				if (!validPath(path)) continue
				const name = await basename(path)
				newFiles.push({ name, path })
			}
			setFiles(newFiles)
			if (newFiles.length === 1) navigate('/', { state: { files: newFiles } })
		}
	}

	async function handleDrop() {
		listen<{ paths: string[] }>('tauri://drag-drop', async (event) => {
			const newFiles: NamedPath[] = []
			for (const path of event.payload.paths) {
				const file = await pathToNamedPath(path)
				newFiles.push({ name: file.name, path: file.path })
			}
			setFiles([
				...newFiles.filter((f) => {
					const value = f.path.toLowerCase()
					return (
						config.videoExtensions.some((ext) => value.endsWith(ext.toLowerCase())) ||
						config.audioExtensions.some((ext) => value.endsWith(ext.toLowerCase())) ||
						files.includes(f)
					)
				}),
			])
			if (newFiles.length === 1) navigate('/', { state: { files: newFiles } })
		})
	}

	async function start() {
		if (inProgress) return
		isAbortingRef.current = false

		let diagnosticRunId: string | null = null
		let completedCount = 0
		let failedCount = 0
		let skippedCount = 0
		let localIndex = 0
		const loopStartTime = performance.now()

		try {
			diagnosticRunId = await startDiagnosticRun('batch', {
				file_count: files.length,
				files: files.map((file) => ({ name: file.name, path: file.path })),
				formats,
				output_folder: outputFolder || null,
				model_path: preference.modelPath,
				model_engine: preference.modelMetadata?.capabilities.engine,
				gpu_device: preference.gpuDevice,
				diarization: preference.diarizeEnabled,
				stable_timestamps: preference.stableTimestampsEnabled,
				chunking_enabled: preference.modelOptions.chunking_enabled !== false,
				llm_summary_enabled: Boolean(preference.llmConfig?.enabled),
			})
			await Promise.all(
				files.map((file, index) =>
					updateDiagnosticItem(diagnosticRunId!, `file-${index + 1}`, file.name, 'queued', { path: file.path }),
				),
			)
		} catch (error) {
			console.error('failed to initialize batch diagnostics', error)
			diagnosticRunId = null
		}

		const record = async (stage: string, message: string, data: Record<string, unknown> = {}, severity: 'info' | 'warning' | 'error' = 'info') => {
			if (!diagnosticRunId) return
			try {
				await recordDiagnosticEvent(diagnosticRunId, stage, message, data, severity, 'batch')
			} catch (error) {
				console.error('failed to write batch diagnostic event', error)
			}
		}

		const avx2 = await invoke<boolean>('is_avx2_enabled')
		if (!avx2) {
			await record('batch.preflight_failed', 'AVX2 preflight check failed', {}, 'error')
			if (diagnosticRunId) await finishDiagnosticRun(diagnosticRunId, 'failed', { reason: 'avx2_not_supported' }).catch(console.error)
			trackAnalyticsEvent(analyticsEvents.AVX2_NOT_SUPPORTED)
			await dialog.message(m.avx2NotSupported(), { kind: 'error' })
			return
		}

		setInProgress(true)
		startKeepAwake()

		try {
			if (!preference.modelPath) throw new Error('No model selected. Please download or select a model first.')
			await record('batch.model_load_started', 'Loading model before batch processing', {
				model_path: preference.modelPath,
				gpu_device: preference.gpuDevice,
			})
			const loadResult = await invoke<string>('load_model', {
				modelPath: preference.modelPath,
				gpuDevice: preference.gpuDevice,
				unloadTimeoutMinutes: preference.unloadTimeoutMinutes,
			})
			await record('batch.model_load_completed', 'Model is ready for batch processing', { result: loadResult })
			if (loadResult === 'gpu_fallback') toast.warning(m.gpuFallbackToCpu(), { position: 'bottom-center', duration: 8000 })

			let diarize_model: string | undefined
			if (preference.diarizeEnabled) {
				const modelsFolder = await invoke<string>('get_models_folder')
				diarize_model = modelsFolder + '/' + config.diarizeModelFilename
			}
			let vad_model: string | undefined
			if (preference.stableTimestampsEnabled || preference.modelMetadata?.capabilities.requires_vad) {
				const modelsFolder = await invoke<string>('get_models_folder')
				vad_model = modelsFolder + '/' + config.vadModelFilename
			}
			setCurrentIndex(localIndex)

			for (let fileIndex = 0; fileIndex < files.length; fileIndex += 1) {
				const file = files[fileIndex]
				const itemId = `file-${fileIndex + 1}`
				if (isAbortingRef.current) break
				setProgress(null)
				const originalPath = file.path
				const startTime = performance.now()
				if (diagnosticRunId) {
					await updateDiagnosticItem(diagnosticRunId, itemId, file.name, 'running', { path: originalPath }).catch(console.error)
				}

				try {
					const someFormat = formatExtensions[formats[0]]
					const ext = await path.extname(originalPath)
					let dst = originalPath.slice(0, -ext.length - 1) + someFormat
					const baseName = await path.basename(dst)
					if (!preference.advancedTranscribeOptions.saveNextToAudioFile && outputFolder) dst = await path.join(outputFolder, baseName)

					if (preference.advancedTranscribeOptions.skipIfExists && !outputFolder && (await fs.exists(dst))) {
						skippedCount += 1
						localIndex += 1
						if (diagnosticRunId) {
							await updateDiagnosticItem(diagnosticRunId, itemId, file.name, 'skipped', {
								reason: 'output_exists',
								output_path: dst,
							}).catch(console.error)
						}
						setCurrentIndex(localIndex)
						continue
					}

					trackAnalyticsEvent(analyticsEvents.TRANSCRIBE_STARTED, { source: 'batch' })
					const options = {
						path: originalPath,
						...preference.modelOptions,
						...(diarize_model ? { diarize_model } : {}),
						...(vad_model ? { vad_model } : {}),
						...(preference.stableTimestampsEnabled ? { stable_timestamps: true } : {}),
						...(diagnosticRunId
							? { diagnostic_run_id: diagnosticRunId, diagnostic_item_id: itemId, diagnostic_source: 'batch' }
							: {}),
					}
					const res: Transcript = await invoke('transcribe', { options })
					const total = Math.round((performance.now() - startTime) / 1000)
					trackAnalyticsEvent(analyticsEvents.TRANSCRIBE_SUCCEEDED, {
						source: 'batch',
						duration_seconds: total,
						segments_count: res.segments.length,
					})

					let llmSegments: Segment[] | null = null
					if (llm && preference.llmConfig?.enabled) {
						await record('batch.summary_started', 'Starting optional LLM summary', { item_id: itemId, file: file.name })
						try {
							const question = `${preference.llmConfig.prompt.replace('%s', transcript.asText(res.segments, speakerLabel))}`
							const answer = await llm.ask(question)
							if (answer) llmSegments = [{ start: 0, stop: res.segments.at(-1)?.stop ?? 0, text: answer }]
							await record('batch.summary_completed', 'LLM summary completed', { item_id: itemId, produced_summary: Boolean(answer) })
						} catch (error) {
							await record('batch.summary_failed', 'LLM summary failed; transcript export will continue', { item_id: itemId, error: normalizeError(error) }, 'warning')
							toast.error(String(error))
							console.error(error)
						}
					}

					for (const format of formats) {
						const exportPath = await invoke<string>('get_path_dst', { src: dst, suffix: formatExtensions[format] })
						await record('batch.export_started', 'Writing transcript export', { item_id: itemId, format, path: exportPath })
						if (format === 'docx') {
							const fileName = await path.basename(exportPath)
							const doc = await toDocx(fileName, res.segments, preference.textAreaDirection, speakerLabel)
							const arrayBuffer = await doc.arrayBuffer()
							await fs.writeFile(exportPath, new Uint8Array(arrayBuffer))
						} else {
							await fs.writeTextFile(exportPath, getText(res.segments, format))
						}
						await record('batch.export_completed', 'Transcript export written', { item_id: itemId, format, path: exportPath })
					}
					if (llmSegments) {
						const summaryPath = await invoke<string>('get_path_dst', { src: dst, suffix: '.summary.txt' })
						await fs.writeTextFile(summaryPath, getText(llmSegments, 'srt'))
						await record('batch.summary_exported', 'LLM summary file written', { item_id: itemId, path: summaryPath })
					}

					completedCount += 1
					localIndex += 1
					if (diagnosticRunId) {
						await updateDiagnosticItem(diagnosticRunId, itemId, file.name, 'succeeded', {
							processing_seconds: total,
							segments: res.segments.length,
							formats,
							summary_generated: Boolean(llmSegments),
						}).catch(console.error)
					}
					await new Promise((resolve) => setTimeout(resolve, 100))
					setCurrentIndex(localIndex)
				} catch (error) {
					const errorObj = typeof error === 'object' && error !== null ? (error as { code?: string; message?: string }) : null
					const errorCode = errorObj?.code
					const errorMessage = errorObj?.message || String(error)

					if (isAbortingRef.current || errorCode === 'aborted') {
						if (diagnosticRunId) {
							await updateDiagnosticItem(diagnosticRunId, itemId, file.name, 'aborted', {}, error).catch(console.error)
						}
						break
					}

					failedCount += 1
					if (diagnosticRunId) {
						await updateDiagnosticItem(diagnosticRunId, itemId, file.name, 'failed', { stage: 'transcribe_or_export' }, error).catch(console.error)
						await record('batch.item_failed', 'Batch item failed during transcription, summarization, or export', {
							item_id: itemId,
							file: file.name,
							error: normalizeError(error),
						}, 'error')
					}

					if (errorCode && isUserError(errorCode)) {
						toast.error(`${m.error()}: ${errorMessage}`)
						console.error(`skipping file ${file.name} due to user error: `, error)
					} else {
						trackAnalyticsEvent(analyticsEvents.TRANSCRIBE_FAILED, {
							source: 'batch',
							error_message: errorMessage,
							file_ext: file.name.split('.').pop() ?? 'unknown',
						})
						console.error(`error while transcribe ${file.name}: `, error)
						if (String(error).includes('no model loaded')) {
							toast.error(m.noModelLoadedBatchStopped())
							break
						}
					}
					localIndex += 1
					setCurrentIndex(localIndex)
				}
			}
		} catch (error) {
			failedCount += 1
			await record('batch.failed', 'Batch failed before or outside an individual item', { error: normalizeError(error) }, 'error')
			console.error('batch processing failed', error)
			toast.error(String(error))
		} finally {
			const wasAborted = isAbortingRef.current
			const elapsedSeconds = Math.round((performance.now() - loopStartTime) / 1000)
			if (diagnosticRunId) {
				const outcome = wasAborted ? 'aborted' : failedCount > 0 ? 'partial' : 'succeeded'
				await finishDiagnosticRun(diagnosticRunId, outcome, {
					files_total: files.length,
					completed: completedCount,
					failed: failedCount,
					skipped: skippedCount,
					elapsed_seconds: elapsedSeconds,
				}).catch(console.error)
			}

			stopKeepAwake()
			if (!wasAborted) setCurrentIndex(files.length + 1)
			setInProgress(false)
			setIsAborting(false)
			setProgress(null)
			isAbortingRef.current = false

			if (wasAborted) {
				console.info(`Batch transcription aborted after ${localIndex} completed/visited files.`)
				navigate('/')
				return
			}
			if (preference.soundOnFinish) new Audio(successSound).play()
			if (preference.focusOnFinish) {
				webview.getCurrentWebviewWindow().unminimize()
				webview.getCurrentWebviewWindow().setFocus()
			}
			console.info(`Batch finished in ${elapsedSeconds} seconds: ${completedCount} completed, ${failedCount} failed, ${skippedCount} skipped.`)
		}
	}

	async function ListenForProgress() {
		await listen<number>('transcribe_progress', (event) => {
			const value = event.payload
			if (value >= 0 && value <= 100) setProgress(value)
		})
	}

	useEffect(() => {
		handleDrop()
		ListenForProgress()
	}, [])

	async function cancel() {
		if (isAbortingRef.current) return
		isAbortingRef.current = true
		emit('abort_transcribe')
		setIsAborting(true)
		setInProgress(false)
	}

	return {
		selectFiles,
		isAborting,
		inProgress,
		setInProgress,
		progress,
		setProgress,
		currentIndex,
		cancel,
		start,
		files,
		formats,
		setFormats,
		preference,
	}
}
