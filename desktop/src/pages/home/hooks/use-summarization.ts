import { useEffect, useState } from 'react'
import { m } from '~/paraglide/messages.js'
import { toast } from 'sonner'
import { useLocalStorage } from 'usehooks-ts'
import { Claude, type Llm, Ollama, OpenAICompatible } from '~/lib/llm'
import * as transcript from '~/lib/transcript'
import { usePreferenceProvider } from '~/providers/preference'
import { finishDiagnosticRun, normalizeError, recordDiagnosticEvent, startDiagnosticRun } from '~/lib/diagnostics'

export function useSummarization() {
	const preference = usePreferenceProvider()
	const [llm, setLlm] = useState<Llm | null>(null)
	const [segments, setSegments] = useState<transcript.Segment[] | null>(null)
	const [summarizing, setSummarizing] = useState(false)
	const [transcriptTab, setTranscriptTab] = useLocalStorage<'transcript' | 'summary'>('prefs_transcript_tab', 'transcript')

	useEffect(() => {
		const config = preference.llmConfig
		setLlm(config.platform === 'ollama' ? new Ollama(config) : config.platform === 'openai' ? new OpenAICompatible(config) : new Claude(config))
	}, [preference.llmConfig])

	async function summarize(source: transcript.Segment[], prompt: string, showSummary = false, parentDiagnosticRunId?: string) {
		if (!llm) return false
		setSummarizing(true)
		const startedAt = performance.now()
		const ownsDiagnosticRun = !parentDiagnosticRunId
		let diagnosticRunId: string | null = parentDiagnosticRunId ?? null

		if (!diagnosticRunId) {
			try {
				diagnosticRunId = await startDiagnosticRun('home_summary', {
					provider: preference.llmConfig.platform,
					model: preference.llmConfig.model,
					source_segment_count: source.length,
					source_character_count: transcript.asText(source, m.speakerPrefix()).length,
					prompt_template_length: prompt.length,
					show_summary_after_completion: showSummary,
				})
			} catch (error) {
				console.error('failed to initialize summary diagnostics', error)
			}
		}

		try {
			const question = prompt.replace('%s', transcript.asText(source, m.speakerPrefix()))
			if (diagnosticRunId) {
				await recordDiagnosticEvent(
					diagnosticRunId,
					'summary.request_started',
					'Sending transcript to the configured LLM summarizer',
					{
						provider: preference.llmConfig.platform,
						model: preference.llmConfig.model,
						question_length: question.length,
						source_segment_count: source.length,
					},
					'info',
					'llm',
				)
			}
			const answerPromise = llm.ask(question)
			toast.promise(answerPromise, {
				loading: m.summarizeLoading(),
				error: (error) => String(error),
				success: m.summarizeSuccess(),
			})
			const answer = await answerPromise
			if (answer) {
				setSegments([{ start: 0, stop: source[source.length - 1]?.stop ?? 0, text: answer }])
				if (showSummary) setTranscriptTab('summary')
			}
			if (diagnosticRunId) {
				await recordDiagnosticEvent(
					diagnosticRunId,
					'summary.request_completed',
					'LLM summarization completed',
					{
						answer_present: Boolean(answer),
						answer_length: answer?.length ?? 0,
						elapsed_ms: Math.round(performance.now() - startedAt),
					},
					'info',
					'llm',
				)
				if (ownsDiagnosticRun) {
					await finishDiagnosticRun(diagnosticRunId, 'succeeded', {
						answer_present: Boolean(answer),
						answer_length: answer?.length ?? 0,
						elapsed_ms: Math.round(performance.now() - startedAt),
					})
				}
			}
			return true
		} catch (error) {
			console.error(error)
			if (diagnosticRunId) {
				await recordDiagnosticEvent(
					diagnosticRunId,
					'summary.request_failed',
					'LLM summarization failed',
					{ error: normalizeError(error), elapsed_ms: Math.round(performance.now() - startedAt) },
					'error',
					'llm',
				).catch(console.error)
				if (ownsDiagnosticRun) {
					await finishDiagnosticRun(diagnosticRunId, 'failed', {
						error: normalizeError(error),
						elapsed_ms: Math.round(performance.now() - startedAt),
					}).catch(console.error)
				}
			}
			return false
		} finally {
			setSummarizing(false)
		}
	}

	return {
		segments,
		setSegments,
		summarizing,
		transcriptTab,
		setTranscriptTab,
		summarize,
	}
}
