cask "opencomputeruse" do
  version "0.9.0"
  sha256 "7da26d8e2bd709c7db5c9f805356a86aea4112744556fcb6cde74bbae3498ece"

  url "https://github.com/IAmJSD/OpenComputerUse/releases/download/v#{version}/OpenComputerUse.zip"
  name "OpenComputerUse"
  desc "Background computer use for agents, as an MCP server"
  homepage "https://github.com/IAmJSD/OpenComputerUse"

  livecheck do
    url :url
    strategy :github_latest
  end

  auto_updates true
  depends_on macos: :sonoma

  app "OpenComputerUse.app"
  binary "#{appdir}/OpenComputerUse.app/Contents/MacOS/opencomputeruse"

  zap trash: "~/Library/Application Support/OpenComputerUse"
end
