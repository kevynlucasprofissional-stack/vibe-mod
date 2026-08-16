import { invoke } from '@tauri-apps/api/core'

export type DiagnosticOutcome = 'succeeded' | 'failed' | 'aborted' | 'skipped' | 'partial'
export type DiagnosticSeverity = 'debug' | 'info' | 'warning' | 'error'

export interface DiagnosticReportPaths {
	json: string
	markdown: string
}

export async function startDiagnosticRun(source: string, context: Record<string, unknown> = {}) {
	return invoke<string>('diagnostics_start_run', { source, context })
}

export async function recordDiagnosticEvent(
	runId: string,
	stage: string,
	message: string,
	data: Record<string, unknown> = {},
	severity: DiagnosticSeverity = 'info',
	category = 'frontend',
) {
	await invoke('diagnostics_record_event', { runId, severity, category, stage, message, data })
}

export async function updateDiagnosticItem(
	runId: string,
	itemId: string,
	label: string,
	status: DiagnosticOutcome | 'queued' | 'running',
	data: Record<string, unknown> = {},
	error?: unknown,
) {
	await invoke('diagnostics_upsert_item', {
		runId,
		itemId,
		label,
		status,
		data,
		error: error == null ? null : normalizeError(error),
	})
}

export async function finishDiagnosticRun(
	runId: string,
	outcome: DiagnosticOutcome,
	result: Record<string, unknown> = {},
) {
	return invoke<DiagnosticReportPaths | null>('diagnostics_finish_run', { runId, outcome, result })
}

export async function latestDiagnosticReport() {
	return invoke<DiagnosticReportPaths | null>('get_latest_diagnostic_report')
}

export async function latestDiagnosticReportContent() {
	return invoke<string | null>('get_latest_diagnostic_report_content')
}

export async function showDiagnosticsFolder() {
	await invoke('show_diagnostics_folder')
}

export async function showLatestDiagnosticReport() {
	return invoke<boolean>('show_latest_diagnostic_report')
}

export function normalizeError(error: unknown) {
	if (error instanceof Error) {
		return {
			name: error.name,
			message: error.message,
			stack: error.stack,
		}
	}
	if (typeof error === 'object' && error !== null) {
		const value = error as Record<string, unknown>
		return {
			code: value.code,
			message: value.message ?? String(error),
			stack: value.stack,
		}
	}
	return { message: String(error) }
}

export async function captureUnexpectedFrontendFailure(kind: 'window_error' | 'unhandled_rejection', error: unknown, metadata: Record<string, unknown> = {}) {
	try {
		const runId = await startDiagnosticRun('frontend_failure', {
			kind,
			error: normalizeError(error),
			...metadata,
		})
		await recordDiagnosticEvent(
			runId,
			`frontend.${kind}`,
			'Unexpected frontend failure escaped normal operation handling',
			{ error: normalizeError(error), ...metadata },
			'error',
			'frontend',
		)
		await finishDiagnosticRun(runId, 'failed', { kind, error: normalizeError(error), ...metadata })
	} catch (diagnosticError) {
		// Never throw from the global failure reporter or it could create a
		// recursive unhandled-rejection loop while the app is already unhealthy.
		console.error('failed to persist unexpected frontend diagnostic', diagnosticError)
	}
}
