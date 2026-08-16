import ReactDOM from 'react-dom/client'
import './globals.css'
import { captureUnexpectedFrontendFailure } from './lib/diagnostics'
import { runMigrations } from './lib/migrations'
import Root from './root'

let isCapturingFrontendFailure = false

function captureFrontendFailure(kind: 'window_error' | 'unhandled_rejection', error: unknown, metadata: Record<string, unknown> = {}) {
	if (isCapturingFrontendFailure) return
	isCapturingFrontendFailure = true
	void captureUnexpectedFrontendFailure(kind, error, metadata).finally(() => {
		isCapturingFrontendFailure = false
	})
}

window.addEventListener('error', (event) => {
	captureFrontendFailure('window_error', event.error ?? event.message, {
		filename: event.filename,
		line: event.lineno,
		column: event.colno,
	})
})

window.addEventListener('unhandledrejection', (event) => {
	captureFrontendFailure('unhandled_rejection', event.reason)
})

runMigrations()

ReactDOM.createRoot(document.getElementById('root') as HTMLElement).render(<Root />)
