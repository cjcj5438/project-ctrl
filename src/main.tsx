import React from 'react'
import ReactDOM from 'react-dom/client'
import App from './App'
import { applyTheme, loadTheme } from './theme'
import './index.css'

// 首帧之前就把主题定下来，避免浅色闪一下
applyTheme(loadTheme())

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
)
