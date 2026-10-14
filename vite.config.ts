import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'
import { defineConfig } from 'vite'

export default defineConfig({
  clearScreen: false,
  plugins: [react(), tailwindcss()],
  server: { port: 1420, strictPort: true },
})
